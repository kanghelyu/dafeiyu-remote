//! `dfy genkey`：生成本机设备的 Ed25519 身份密钥对。
//!
//! - 私钥 = 32 字节 Ed25519 seed（hex 落盘到 `device.key`）
//! - 公钥 = 32 字节 Ed25519 公钥（hex 落盘到 `device.pub`，可安全分发）
//! - device key id = `BLAKE3(公钥)[..16]` 的 RFC4648 base32 小写（26 字符）
//!
//! 生产实现应把私钥放进 OS 密钥库（Windows DPAPI / Credential Manager）而非明文文件；
//! 这里为了 demo 与自测的可复现性选择明文 + 明确告警。

use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use ed25519_dalek::SigningKey;

/// device key id 取的 BLAKE3 前缀字节数。
const KEY_ID_BYTES: usize = 16;

pub fn run(out_dir: &Path) -> Result<()> {
    fs::create_dir_all(out_dir)
        .with_context(|| format!("无法创建输出目录 {}", out_dir.display()))?;

    let signing_key = SigningKey::generate(&mut rand::rngs::OsRng);
    let verifying_key = signing_key.verifying_key();
    let public = verifying_key.to_bytes();

    let key_path = out_dir.join("device.key");
    let pub_path = out_dir.join("device.pub");
    write_hex_file(&key_path, &signing_key.to_bytes())
        .with_context(|| format!("写入私钥 {} 失败", key_path.display()))?;
    write_hex_file(&pub_path, &public)
        .with_context(|| format!("写入公钥 {} 失败", pub_path.display()))?;

    let digest = blake3::hash(&public);
    let key_id = base32_lower(&digest.as_bytes()[..KEY_ID_BYTES]);

    println!("device key id: {key_id}");
    println!(
        "private key : {}（32B Ed25519 seed，hex，**明文**）",
        key_path.display()
    );
    println!(
        "public key  : {}（32B Ed25519 公钥，hex）",
        pub_path.display()
    );
    println!("提示: 私钥明文落盘仅用于 demo/自测；生产应放入 OS 密钥库或加密存储");
    Ok(())
}

/// 写一行 hex 文本（README 可读，便于 demo 反复使用同一密钥）。
fn write_hex_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file =
        fs::File::create(path).with_context(|| format!("无法创建 {}", path.display()))?;
    writeln!(file, "{}", hex::encode(bytes)).context("写入 hex 内容失败")?;
    file.flush().context("刷新文件失败")?;
    Ok(())
}

/// RFC 4648 base32（小写字母表，无填充）。
fn base32_lower(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut out = String::with_capacity(bytes.len().div_ceil(5) * 8);
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    for &byte in bytes {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(char::from(ALPHABET[((buffer >> bits) & 0x1f) as usize]));
        }
    }
    if bits > 0 {
        out.push(char::from(
            ALPHABET[((buffer << (5 - bits)) & 0x1f) as usize],
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base32_matches_rfc4648_lowercase() {
        // RFC 4648 §10 测试向量 "f" -> "my"
        assert_eq!(base32_lower(b""), "");
        assert_eq!(base32_lower(b"f"), "my");
        assert_eq!(base32_lower(b"fo"), "mzxq");
        assert_eq!(base32_lower(b"foo"), "mzxw6");
        assert_eq!(base32_lower(b"foobar"), "mzxw6ytboi");
    }

    #[test]
    fn key_id_is_26_chars_from_16_bytes() {
        assert_eq!(base32_lower(&[0u8; KEY_ID_BYTES]).len(), 26);
        assert_eq!(base32_lower(&[0xffu8; KEY_ID_BYTES]).len(), 26);
    }

    #[test]
    fn genkey_writes_hex_files() {
        let dir = std::env::temp_dir().join(format!(
            "dfy-genkey-test-{}-{}",
            std::process::id(),
            dfy_transport::now_us()
        ));
        run(&dir).expect("genkey 应成功");
        let key = fs::read_to_string(dir.join("device.key")).expect("读私钥");
        let pubkey = fs::read_to_string(dir.join("device.pub")).expect("读公钥");
        assert_eq!(key.trim().len(), 64, "私钥 hex 长度");
        assert_eq!(pubkey.trim().len(), 64, "公钥 hex 长度");
        assert!(hex::decode(key.trim()).is_ok());
        assert!(hex::decode(pubkey.trim()).is_ok());
        let _ = fs::remove_dir_all(&dir);
    }
}
