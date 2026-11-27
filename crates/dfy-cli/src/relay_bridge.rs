//! 中继模式接入：把一个已配对的 UDP socket 交给 QUIC 端点。
//!
//! 设计见 `design/022-relay-client.md`：中继是「按 tag 成对转发的 UDP 会合点」，不解析 QUIC、
//! 不终结 TLS。因此 agent/view 只需把中继 socket 传给 quinn，握手与会话层完全不变；
//! 两侧看到的对端地址都是中继地址（NAT 后才可通过）。
//!
//! tag 由两端通过 `--relay-tag` 显式共享（16 字节 hex），不再从会话口令派生。

use std::net::SocketAddr;

use anyhow::{Context, Result};
use dfy_relay::config::RelayKey;
use dfy_relay::{bind_relay_socket, RelayClientConfig, RelaySocket, Role};

/// 中继连接参数。
#[derive(Debug, Clone)]
pub struct RelayOptions {
    /// 中继 UDP 地址。
    pub addr: SocketAddr,
    /// 会合 tag（16 字节）。
    pub tag: [u8; 16],
    /// 配对校验密钥（与中继服务器 `relay_key` 一致）。
    pub key: RelayKey,
}

impl RelayOptions {
    /// 从命令行参数构造：`tag_hex` 为两端显式共享的 16 字节 tag。
    pub fn new(addr: SocketAddr, tag_hex: &str, key_text: &str) -> Result<Self> {
        let tag = parse_tag_hex(tag_hex)?;
        let key = RelayKey::parse(key_text).context("解析 --relay-key 失败")?;
        Ok(Self { addr, tag, key })
    }

    /// 以某一角色绑定中继 socket（内部完成 BindRequest/BindOk 与 20s 保活）。
    pub async fn bind(&self, role: Role) -> Result<RelaySocket> {
        let cfg = RelayClientConfig {
            relay_addr: self.addr,
            tag: self.tag,
            role,
            key: *self.key.as_bytes(),
            nonce: dfy_transport::now_us(),
        };
        bind_relay_socket(&cfg)
            .await
            .with_context(|| format!("与中继 {} 完成 tag 配对失败", self.addr))
    }
}

/// 解析 32 位 hex 的 tag。
pub fn parse_tag_hex(text: &str) -> Result<[u8; 16]> {
    let cleaned: String = text
        .chars()
        .filter(|c| !matches!(c, ':' | '-' | ' ' | '\t'))
        .collect();
    let bytes = hex::decode(&cleaned).with_context(|| format!("tag 不是合法 hex：{text}"))?;
    anyhow::ensure!(
        bytes.len() == 16,
        "tag 必须是 16 字节（32 位 hex），实际 {} 字节",
        bytes.len()
    );
    let mut tag = [0u8; 16];
    tag.copy_from_slice(&bytes);
    Ok(tag)
}

/// tag 的展示形式（hex，便于两端核对）。
pub fn tag_hex(tag: &[u8; 16]) -> String {
    tag.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tag_hex_accepts_separators() {
        let tag = parse_tag_hex("00:11:22:33:44:55:66:77:88:99:aa:bb:cc:dd:ee:ff").expect("parse");
        assert_eq!(tag[0], 0x00);
        assert_eq!(tag[15], 0xff);
        assert!(parse_tag_hex("00:11").is_err());
    }
}
