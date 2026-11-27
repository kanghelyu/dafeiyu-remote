// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Dafeiyu Remote contributors
// Copyright (c) 2026 Dafeiyu Remote contributors
//! DFYP/1-Relay 信封协议（clean-room 自定字段，详见 DESIGN-NOTES.md）。
//!
//! 所有控制报文以 6 字节头部开始：`magic(4) | ver(1) | kind(1)`。
//! 数据面报文**不带**中继头部：bind 之后任何非控制报文按源地址查表原样转发，
//! 这样客户端可以直接把 QUIC 密文丢进同一 socket，中继不改包体。
//!
//! ```text
//! Bind     : magic ver=1 kind=1 tag(16) role(1) nonce(16) ts_ms(8 BE) auth(32)  = 79B
//! BindOk   : magic ver=1 kind=2 tag(16) expires_in_s(4 BE) server_ms(8 BE)      = 34B
//! BindErr  : magic ver=1 kind=3 tag(16) code(1)                                 = 23B
//! Bye      : magic ver=1 kind=4 tag(16)                                         = 22B
//! TCP 帧    : u32 len(BE, 含 chan 字节) | u8 chan | payload
//! ```

pub const MAGIC: [u8; 4] = *b"DFYR";
pub const VERSION: u8 = 1;

pub const KIND_BIND: u8 = 1;
pub const KIND_BIND_OK: u8 = 2;
pub const KIND_BIND_ERR: u8 = 3;
pub const KIND_BYE: u8 = 4;

pub const HEADER_LEN: usize = 6;
pub const TAG_LEN: usize = 16;
pub const NONCE_LEN: usize = 16;
pub const AUTH_LEN: usize = 32;
pub const BIND_LEN: usize = HEADER_LEN + TAG_LEN + 1 + NONCE_LEN + 8 + AUTH_LEN;
pub const BIND_OK_LEN: usize = HEADER_LEN + TAG_LEN + 4 + 8;
pub const BIND_ERR_LEN: usize = HEADER_LEN + TAG_LEN + 1;
pub const BYE_LEN: usize = HEADER_LEN + TAG_LEN;

/// TCP 帧信道：0=数据（原 QUIC 包），1=心跳，2=控制（bind/bye）。
pub const CHAN_DATA: u8 = 0;
pub const CHAN_PING: u8 = 1;
pub const CHAN_CTRL: u8 = 2;

/// TCP 单帧上限（含 chan 字节）。
pub const MAX_TCP_FRAME: usize = 64 * 1024;

pub type Tag = [u8; TAG_LEN];
pub type Nonce = [u8; NONCE_LEN];
pub type Auth = [u8; AUTH_LEN];

pub const ERR_TAG_FULL: u8 = 1;
pub const ERR_BUSY: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindPacket {
    pub tag: Tag,
    pub role: u8,
    pub nonce: Nonce,
    pub ts_ms: u64,
    pub auth: Auth,
}

fn header(kind: u8) -> [u8; HEADER_LEN] {
    let mut h = [0u8; HEADER_LEN];
    h[..4].copy_from_slice(&MAGIC);
    h[4] = VERSION;
    h[5] = kind;
    h
}

pub fn bind_packet(tag: &Tag, role: u8, nonce: &Nonce, ts_ms: u64, auth: &Auth) -> Vec<u8> {
    let mut out = Vec::with_capacity(BIND_LEN);
    out.extend_from_slice(&header(KIND_BIND));
    out.extend_from_slice(tag);
    out.push(role);
    out.extend_from_slice(nonce);
    out.extend_from_slice(&ts_ms.to_be_bytes());
    out.extend_from_slice(auth);
    out
}

pub fn parse_bind(buf: &[u8]) -> Option<BindPacket> {
    if buf.len() != BIND_LEN || buf[..4] != MAGIC || buf[4] != VERSION || buf[5] != KIND_BIND {
        return None;
    }
    let mut tag = [0u8; TAG_LEN];
    tag.copy_from_slice(&buf[HEADER_LEN..HEADER_LEN + TAG_LEN]);
    let role = buf[HEADER_LEN + TAG_LEN];
    let mut nonce = [0u8; NONCE_LEN];
    let n0 = HEADER_LEN + TAG_LEN + 1;
    nonce.copy_from_slice(&buf[n0..n0 + NONCE_LEN]);
    let t0 = n0 + NONCE_LEN;
    let ts_ms = u64::from_be_bytes(buf[t0..t0 + 8].try_into().ok()?);
    let mut auth = [0u8; AUTH_LEN];
    auth.copy_from_slice(&buf[t0 + 8..t0 + 8 + AUTH_LEN]);
    Some(BindPacket {
        tag,
        role,
        nonce,
        ts_ms,
        auth,
    })
}

pub fn bind_ok_packet(tag: &Tag, expires_in_s: u32, server_ms: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(BIND_OK_LEN);
    out.extend_from_slice(&header(KIND_BIND_OK));
    out.extend_from_slice(tag);
    out.extend_from_slice(&expires_in_s.to_be_bytes());
    out.extend_from_slice(&server_ms.to_be_bytes());
    out
}

pub fn bind_err_packet(tag: &Tag, code: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(BIND_ERR_LEN);
    out.extend_from_slice(&header(KIND_BIND_ERR));
    out.extend_from_slice(tag);
    out.push(code);
    out
}

pub fn bye_packet(tag: &Tag) -> Vec<u8> {
    let mut out = Vec::with_capacity(BYE_LEN);
    out.extend_from_slice(&header(KIND_BYE));
    out.extend_from_slice(tag);
    out
}

/// TCP 帧编码：`u32 len(BE) | u8 chan | payload`，`len` 包含 chan 字节。
pub fn tcp_frame(chan: u8, payload: &[u8]) -> Vec<u8> {
    let len = payload.len() + 1;
    let mut out = Vec::with_capacity(4 + len);
    out.extend_from_slice(&(len as u32).to_be_bytes());
    out.push(chan);
    out.extend_from_slice(payload);
    out
}

/// 解析控制报文类型；非控制包（纯数据）返回 `Data`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketClass {
    Bind,
    BindOk,
    BindErr,
    Bye,
    /// 无中继头部，按数据面处理。
    Data,
    /// 头部像控制帧但长度/内容非法。
    Malformed,
}

pub fn classify(buf: &[u8]) -> PacketClass {
    if buf.len() < HEADER_LEN || buf[..4] != MAGIC || buf[4] != VERSION {
        return PacketClass::Data;
    }
    match buf[5] {
        KIND_BIND => {
            if buf.len() == BIND_LEN {
                PacketClass::Bind
            } else {
                PacketClass::Malformed
            }
        }
        KIND_BIND_OK => {
            if buf.len() == BIND_OK_LEN {
                PacketClass::BindOk
            } else {
                PacketClass::Malformed
            }
        }
        KIND_BIND_ERR => {
            if buf.len() == BIND_ERR_LEN {
                PacketClass::BindErr
            } else {
                PacketClass::Malformed
            }
        }
        KIND_BYE => {
            if buf.len() == BYE_LEN {
                PacketClass::Bye
            } else {
                PacketClass::Malformed
            }
        }
        _ => PacketClass::Data,
    }
}

/// 从 Bye 报文取 tag。
pub fn parse_bye_tag(buf: &[u8]) -> Option<Tag> {
    if buf.len() != BYE_LEN || buf[..4] != MAGIC || buf[4] != VERSION || buf[5] != KIND_BYE {
        return None;
    }
    let mut tag = [0u8; TAG_LEN];
    tag.copy_from_slice(&buf[HEADER_LEN..HEADER_LEN + TAG_LEN]);
    Some(tag)
}

/// 日志安全前缀：tag 前 4 字节 hex（8 个字符）。`no_log_ips` 时只允许打这个。
pub fn tag_prefix(tag: &Tag) -> String {
    hex::encode(&tag[..4])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(n: u8) -> Tag {
        [n; TAG_LEN]
    }

    #[test]
    fn bind_roundtrip() {
        let t = tag(7);
        let nonce = [9u8; NONCE_LEN];
        let auth = [3u8; AUTH_LEN];
        let pkt = bind_packet(&t, 1, &nonce, 12345, &auth);
        assert_eq!(pkt.len(), BIND_LEN);
        let p = parse_bind(&pkt).expect("parse");
        assert_eq!(p.tag, t);
        assert_eq!(p.role, 1);
        assert_eq!(p.nonce, nonce);
        assert_eq!(p.ts_ms, 12345);
        assert_eq!(p.auth, auth);
        assert_eq!(classify(&pkt), PacketClass::Bind);
    }

    #[test]
    fn bind_ok_err_bye_layouts() {
        let t = tag(1);
        let ok = bind_ok_packet(&t, 48 * 3600, 99);
        assert_eq!(ok.len(), BIND_OK_LEN);
        assert_eq!(classify(&ok), PacketClass::BindOk);
        let err = bind_err_packet(&t, ERR_TAG_FULL);
        assert_eq!(err.len(), BIND_ERR_LEN);
        assert_eq!(classify(&err), PacketClass::BindErr);
        let bye = bye_packet(&t);
        assert_eq!(bye.len(), BYE_LEN);
        assert_eq!(classify(&bye), PacketClass::Bye);
        assert_eq!(parse_bye_tag(&bye), Some(t));
    }

    #[test]
    fn data_is_passthrough() {
        // 任意字节（例如 QUIC 密文）都按数据面处理。
        assert_eq!(classify(b"\x00\x01\x02"), PacketClass::Data);
        assert_eq!(classify(b""), PacketClass::Data);
        // 头部像 bind 但长度不对 → malformed，不误判为数据。
        let mut bad = bind_packet(&tag(2), 0, &[0u8; NONCE_LEN], 1, &[0u8; AUTH_LEN]);
        bad.truncate(10);
        assert_eq!(classify(&bad), PacketClass::Malformed);
    }

    #[test]
    fn tcp_frame_layout() {
        let f = tcp_frame(CHAN_DATA, b"hello");
        assert_eq!(&f[..4], &6u32.to_be_bytes());
        assert_eq!(f[4], CHAN_DATA);
        assert_eq!(&f[5..], b"hello");
    }
}
