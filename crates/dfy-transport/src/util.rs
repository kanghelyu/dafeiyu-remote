//! 小工具：时间戳、随机数、摘要、指纹/十六进制转换。
//!
//! 口令认证实现位于 crate 内部的 auth 模块；不提供旧 nonce-only 证明原语。

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use rand::RngCore;
use sha2::{Digest, Sha256};

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
