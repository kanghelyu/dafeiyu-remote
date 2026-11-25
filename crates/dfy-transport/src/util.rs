//! 小工具：时间戳、随机数、密码证明、指纹/十六进制转换。
//!
//! 这里的密码证明是 **MVP 简化版**：`HMAC-SHA256(key = SHA-256(password), msg = nonce)`。
//! 正式版应换成 Argon2id 派生密钥后再做 HMAC（见 `spec/DFYP-v1.md` 握手章节）。

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// 当前 Unix 微秒时间戳（握手 nonce 的时间语义、心跳 RTT、视频帧 `ts_us`）。
pub fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// 用 OS 随机源生成 `len` 字节。
pub fn random_bytes(len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    buf
}

/// SHA-256 摘要。
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let out = hasher.finalize();
    let mut digest = [0u8; 32];
    digest.copy_from_slice(&out);
    digest
}

/// SHA-256 摘要的小写十六进制表示。
pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(sha256(data))
}

/// 从口令派生 HMAC 密钥。
///
/// MVP：直接 SHA-256；`// MVP: replace with Argon2id`。
pub fn password_key(password: &str) -> [u8; 32] {
    // MVP: replace with Argon2id
    sha256(password.as_bytes())
}

/// 计算握手证明：`HMAC-SHA256(SHA-256(password), nonce)`。
pub fn password_proof(password: &str, nonce: &[u8]) -> Vec<u8> {
    let key = password_key(password);
    let mut mac = HmacSha256::new_from_slice(&key).expect("HMAC 接受任意长度密钥");
    mac.update(nonce);
    mac.finalize().into_bytes().to_vec()
}

/// 常数时间校验握手证明。
pub fn verify_proof(password: &str, nonce: &[u8], proof: &[u8]) -> bool {
    let key = password_key(password);
    let mut mac = match HmacSha256::new_from_slice(&key) {
        Ok(mac) => mac,
        Err(_) => return false,
    };
    mac.update(nonce);
    mac.verify_slice(proof).is_ok()
}

/// 常数时间字节比较（避免提前返回泄漏比较进度）。
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// 解析 `--expect-fingerprint` 参数（32 字节 = 64 个十六进制字符，允许夹杂 `:`/空格）。
pub fn parse_fingerprint_hex(text: &str) -> Result<[u8; 32]> {
    let cleaned: String = text.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    anyhow::ensure!(
        cleaned.len() == 64,
        "证书指纹应为 32 字节（64 个十六进制字符），实际 {} 个十六进制字符",
        cleaned.len()
    );
    let bytes = hex::decode(&cleaned).map_err(|e| anyhow!("证书指纹不是合法十六进制: {e}"))?;
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

/// 人类可读的字节数。
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proof_is_deterministic_and_password_bound() {
        let nonce = random_bytes(32);
        let proof = password_proof("test", &nonce);
        assert_eq!(proof.len(), 32);
        assert!(verify_proof("test", &nonce, &proof));
        assert!(!verify_proof("wrong", &nonce, &proof));
        assert!(!verify_proof("test", &random_bytes(32), &proof));
    }

    #[test]
    fn fingerprint_roundtrip() {
        let raw = [0xABu8; 32];
        let parsed = parse_fingerprint_hex(&hex::encode(raw)).expect("解析指纹");
        assert_eq!(parsed, raw);
        // 大写 + 冒号分隔也应接受
        let pretty = hex::encode_upper(raw)
            .as_bytes()
            .chunks(2)
            .map(|c| String::from_utf8_lossy(c).to_string())
            .collect::<Vec<_>>()
            .join(":");
        assert_eq!(parse_fingerprint_hex(&pretty).expect("解析美化指纹"), raw);
        assert!(parse_fingerprint_hex("abc").is_err());
    }

    #[test]
    fn ct_eq_matches_normal_equality() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"abcd"));
    }
}
