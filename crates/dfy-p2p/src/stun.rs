// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! RFC 5389 子集：Binding Request / Success Response / Error Response 的编解码，
//! `XOR-MAPPED-ADDRESS` / `MAPPED-ADDRESS` / `RESPONSE-ORIGIN` / `OTHER-ADDRESS` /
//! `CHANGE-REQUEST` / `ERROR-CODE` 属性，以及带超时重传的探测客户端。
//!
//! **clean-room**：只依据 RFC 5389 / RFC 3489 的公开格式表实现，不引入任何 STUN crate。
//!
//! `MESSAGE-INTEGRITY`（HMAC-SHA1）本阶段**只预留结构**：
//! [`StunMessage::decode`] 会把它按未知属性原样保留，[`StunMessage::message_integrity`]
//! 可读出原始 20 字节摘要；发送侧不会附加该属性（公开 STUN 服务器通常允许匿名 Binding）。
//! 待办与理由见 `design/023-p2p-punch.md` §8。
//!
//! ## 报文格式（RFC 5389 §6）
//!
//! ```text
//! 0                   1                   2                   3
//! 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |0 0|     STUN Message Type     |         Message Length        |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |                         Magic Cookie                          |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |                                                               |
//! |                     Transaction ID (96 bits)                  |
//! |                                                               |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |                          Attributes ...                       |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! ```

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6, UdpSocket};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use tokio::net::UdpSocket as TokioUdpSocket;
use tracing::{debug, trace};

/// STUN magic cookie（RFC 5389 §6）。
pub const MAGIC_COOKIE: u32 = 0x2112_A442;
/// 固定头长度：type(2) + length(2) + magic cookie(4) + transaction id(12)。
pub const HEADER_LEN: usize = 20;
/// 事务 ID 字节数（96 bit）。
pub const TRANSACTION_ID_LEN: usize = 12;

/// Binding 方法号。
pub const BINDING_METHOD: u16 = 0x0001;
/// Binding Request（method=0x001, class=request）。
pub const BINDING_REQUEST: u16 = 0x0001;
/// Binding Indication（method=0x001, class=indication）。
pub const BINDING_INDICATION: u16 = 0x0011;
/// Binding Success Response（method=0x001, class=success）。
pub const BINDING_SUCCESS_RESPONSE: u16 = 0x0101;
/// Binding Error Response（method=0x001, class=error）。
pub const BINDING_ERROR_RESPONSE: u16 = 0x0111;

/// `MAPPED-ADDRESS`（RFC 3489 遗留，非 XOR）。
pub const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
/// `CHANGE-REQUEST`（RFC 5780 / 经典 NAT 行为探测）。
pub const ATTR_CHANGE_REQUEST: u16 = 0x0003;
/// `MESSAGE-INTEGRITY`（本阶段仅预留：可读不写）。
pub const ATTR_MESSAGE_INTEGRITY: u16 = 0x0008;
/// `ERROR-CODE`。
pub const ATTR_ERROR_CODE: u16 = 0x0009;
/// `XOR-MAPPED-ADDRESS`（RFC 5389 唯一要求的映射地址属性）。
pub const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
/// `SOFTWARE`。
pub const ATTR_SOFTWARE: u16 = 0x8022;
/// `FINGERPRINT`。
pub const ATTR_FINGERPRINT: u16 = 0x8028;
/// `RESPONSE-ORIGIN`（RFC 5780：响应实际发出的地址）。
pub const ATTR_RESPONSE_ORIGIN: u16 = 0x802b;
/// `OTHER-ADDRESS`（RFC 5780：服务器的备用地址，供 NAT 行为探测）。
pub const ATTR_OTHER_ADDRESS: u16 = 0x802c;

/// CHANGE-REQUEST 的 "change IP" 位。
pub const CHANGE_IP_FLAG: u32 = 0x04;
/// CHANGE-REQUEST 的 "change port" 位。
pub const CHANGE_PORT_FLAG: u32 = 0x02;

/// 一次 STUN 探测最多发送次数：初始 1 次 + 重传 3 次（受总超时截断）。
pub const MAX_STUN_ATTEMPTS: u32 = 4;
/// 重传间隔下限，防止极小 timeout 造成忙等。
pub const MIN_STUN_RETRANSMIT: Duration = Duration::from_millis(100);
/// [`stun_probe`] 的推荐超时（局域网/公网 STUN 通常 < 100ms 往返）。
pub const DEFAULT_STUN_TIMEOUT: Duration = Duration::from_secs(2);

/// STUN 消息类别（method/class 的位交错编码见 RFC 5389 §6）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageClass {
    /// Request（class = 0b00）。
    Request,
    /// Indication（class = 0b01）。
    Indication,
    /// Success Response（class = 0b10）。
    SuccessResponse,
    /// Error Response（class = 0b11）。
    ErrorResponse,
}

impl MessageClass {
    /// 取协议里的 2 bit 类别值。
    pub const fn bits(self) -> u8 {
        match self {
            MessageClass::Request => 0b00,
            MessageClass::Indication => 0b01,
            MessageClass::SuccessResponse => 0b10,
            MessageClass::ErrorResponse => 0b11,
        }
    }

    /// 从 2 bit 类别值还原；越界返回 `None`（实际不会发生，2 bit 全覆盖）。
    pub const fn from_bits(bits: u8) -> Option<Self> {
        match bits & 0b11 {
            0b00 => Some(MessageClass::Request),
            0b01 => Some(MessageClass::Indication),
            0b10 => Some(MessageClass::SuccessResponse),
            _ => Some(MessageClass::ErrorResponse),
        }
    }
}

/// 把 `(method, class)` 编码成 14 bit 的 STUN message type（RFC 5389 §6 的位交错）。
pub const fn encode_message_type(method: u16, class: MessageClass) -> u16 {
    let method = method & 0x0FFF;
    let class = class.bits() as u16;
    (method & 0x000F)
        | ((method & 0x0070) << 1)
        | ((method & 0x0F80) << 2)
        | ((class & 0x0002) << 7)
        | ((class & 0x0001) << 4)
}

/// 从 14 bit message type 解出 `(method, class)`。
pub const fn decode_message_type(msg_type: u16) -> (u16, MessageClass) {
    let method = (msg_type & 0x000F) | ((msg_type & 0x00E0) >> 1) | ((msg_type & 0x3E00) >> 2);
    let class_bits = (((msg_type >> 7) & 0x0002) | ((msg_type >> 4) & 0x0001)) as u8;
    (
        method,
        match class_bits & 0b11 {
            0b00 => MessageClass::Request,
            0b01 => MessageClass::Indication,
            0b10 => MessageClass::SuccessResponse,
            _ => MessageClass::ErrorResponse,
        },
    )
}

/// 原样保留的 STUN 属性（TLV）。未实现的属性（如 `MESSAGE-INTEGRITY`）也走这里。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawAttribute {
    /// 属性类型（IANA STUN registry）。
    pub typ: u16,
    /// 属性值（不含 4 字节对齐填充）。
    pub value: Vec<u8>,
}

impl RawAttribute {
    /// 构造一个原样属性。
    pub fn new(typ: u16, value: impl Into<Vec<u8>>) -> Self {
        Self {
            typ,
            value: value.into(),
        }
    }
}

/// `CHANGE-REQUEST` 想请求服务器从哪个"另一个地址"回包（RFC 5780）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChangeFlags {
    /// 请求服务器换 **IP** 回包（bit 2）。
    pub change_ip: bool,
    /// 请求服务器换 **端口** 回包（bit 1）。
    pub change_port: bool,
}

impl ChangeFlags {
    /// 换 IP + 换端口（最严格的一档）。
    pub const IP_AND_PORT: Self = Self {
        change_ip: true,
        change_port: true,
    };

    /// 是否没有请求任何变化。
    pub const fn is_none(self) -> bool {
        !self.change_ip && !self.change_port
    }

    /// 编码成 CHANGE-REQUEST 的 32 bit 值；无标志时返回 `None`。
    pub const fn to_bits(self) -> Option<u32> {
        if self.is_none() {
            return None;
        }
        let mut bits = 0u32;
        if self.change_ip {
            bits |= CHANGE_IP_FLAG;
        }
        if self.change_port {
            bits |= CHANGE_PORT_FLAG;
        }
        Some(bits)
    }
}

/// 一条 STUN 消息（头 + 属性表）。构造与编码/解码都是纯函数，便于单测和服务端自测。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StunMessage {
    /// 14 bit message type（见 [`encode_message_type`]）。
    pub msg_type: u16,
    /// 96 bit 事务 ID。
    pub transaction_id: [u8; TRANSACTION_ID_LEN],
    /// 属性表（保持线序）。
    pub attributes: Vec<RawAttribute>,
}

impl StunMessage {
    /// 用给定类型与事务 ID 构造空消息。
    pub fn new(msg_type: u16, transaction_id: [u8; TRANSACTION_ID_LEN]) -> Self {
        Self {
            msg_type,
            transaction_id,
            attributes: Vec::new(),
        }
    }

    /// 构造 Binding Request（后续可用 [`Self::with_attribute`] 附加 CHANGE-REQUEST）。
    pub fn binding_request(transaction_id: [u8; TRANSACTION_ID_LEN]) -> Self {
        Self::new(BINDING_REQUEST, transaction_id)
    }

    /// 构造 Binding Success Response：XOR-MAPPED-ADDRESS 必带，OTHER-ADDRESS 可选。
    pub fn binding_success(
        transaction_id: [u8; TRANSACTION_ID_LEN],
        mapped: SocketAddr,
        other: Option<SocketAddr>,
    ) -> Self {
        let mut msg = Self::new(BINDING_SUCCESS_RESPONSE, transaction_id);
        msg.attributes
            .push(xor_mapped_address_attribute(mapped, &transaction_id));
        if let Some(other) = other {
            msg.attributes.push(other_address_attribute(other));
        }
        msg
    }

    /// 构造 Binding Error Response（`code` 例如 400 / 420，`reason` 为 UTF-8 短语）。
    pub fn binding_error(
        transaction_id: [u8; TRANSACTION_ID_LEN],
        code: u16,
        reason: &str,
    ) -> Self {
        let mut msg = Self::new(BINDING_ERROR_RESPONSE, transaction_id);
        msg.attributes.push(error_code_attribute(code, reason));
        msg
    }

    /// 追加一个属性（builder 风格）。
    pub fn with_attribute(mut self, attribute: RawAttribute) -> Self {
        self.attributes.push(attribute);
        self
    }

    /// 取第一个该类型的属性值。
    pub fn attribute(&self, typ: u16) -> Option<&[u8]> {
        self.attributes
            .iter()
            .find(|attr| attr.typ == typ)
            .map(|attr| attr.value.as_slice())
    }

    /// 是否是 Binding Request。
    pub fn is_binding_request(&self) -> bool {
        self.msg_type == BINDING_REQUEST
    }

    /// 是否是 Binding Success Response。
    pub fn is_binding_success(&self) -> bool {
        self.msg_type == BINDING_SUCCESS_RESPONSE
    }

    /// 解析 `XOR-MAPPED-ADDRESS`；若同时存在非 XOR 的 `MAPPED-ADDRESS` 也可读出。
    pub fn xor_mapped_address(&self) -> Result<Option<SocketAddr>> {
        self.attribute(ATTR_XOR_MAPPED_ADDRESS)
            .map(|value| decode_address(value, true, &self.transaction_id))
            .transpose()
    }

    /// 解析 RFC 3489 的 `MAPPED-ADDRESS`（非 XOR）。
    pub fn mapped_address(&self) -> Result<Option<SocketAddr>> {
        self.attribute(ATTR_MAPPED_ADDRESS)
            .map(|value| decode_address(value, false, &self.transaction_id))
            .transpose()
    }

    /// 解析 `RESPONSE-ORIGIN`（响应实际发自哪个地址）。
    pub fn response_origin(&self) -> Result<Option<SocketAddr>> {
        self.attribute(ATTR_RESPONSE_ORIGIN)
            .map(|value| decode_address(value, false, &self.transaction_id))
            .transpose()
    }

    /// 解析 `OTHER-ADDRESS`（STUN 服务器的备用地址）。
    pub fn other_address(&self) -> Result<Option<SocketAddr>> {
        self.attribute(ATTR_OTHER_ADDRESS)
            .map(|value| decode_address(value, false, &self.transaction_id))
            .transpose()
    }

    /// 解析 `CHANGE-REQUEST` 的 32 bit 标志位；不存在返回 `None`。
    pub fn change_request(&self) -> Option<u32> {
        let value = self.attribute(ATTR_CHANGE_REQUEST)?;
        if value.len() < 4 {
            return None;
        }
        Some(u32::from_be_bytes([value[0], value[1], value[2], value[3]]))
    }

    /// 读出 `MESSAGE-INTEGRITY` 的原始摘要（结构预留，不做校验）。
    pub fn message_integrity(&self) -> Option<&[u8]> {
        self.attribute(ATTR_MESSAGE_INTEGRITY)
    }

    /// 解析 `ERROR-CODE` 为 `(三位错误码, 原因短语)`。
    pub fn error_code(&self) -> Option<(u16, String)> {
        let value = self.attribute(ATTR_ERROR_CODE)?;
        if value.len() < 4 {
            return None;
        }
        let class = (value[2] & 0x07) as u16;
        let number = value[3] as u16;
        let reason = String::from_utf8_lossy(&value[4..])
            .trim_end_matches('\0')
            .to_string();
        Some((class * 100 + number, reason))
    }

    /// 按 RFC 5389 编码成字节串（含 4 字节属性对齐填充）。
    pub fn encode(&self) -> Vec<u8> {
        debug_assert!(self.msg_type & 0xC000 == 0, "message type 最高两位必须为 0");
        let mut out = Vec::with_capacity(HEADER_LEN + 32);
        out.extend_from_slice(&self.msg_type.to_be_bytes());
        out.extend_from_slice(&[0u8, 0u8]); // 长度占位，最后回填
        out.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        out.extend_from_slice(&self.transaction_id);

        for attribute in &self.attributes {
            let value_len = attribute.value.len().min(u16::MAX as usize) as u16;
            out.extend_from_slice(&attribute.typ.to_be_bytes());
            out.extend_from_slice(&value_len.to_be_bytes());
            out.extend_from_slice(&attribute.value);
            let padding = (4 - attribute.value.len() % 4) % 4;
            out.extend(std::iter::repeat(0u8).take(padding));
        }

        let body_len = (out.len() - HEADER_LEN) as u16;
        out[2..4].copy_from_slice(&body_len.to_be_bytes());
        out
    }

    /// 从字节串解码；校验头部（长度对齐、magic cookie、边界）与属性 TLV 边界。
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < HEADER_LEN {
            bail!(
                "STUN 消息过短：{} 字节（至少 {HEADER_LEN} 字节）",
                bytes.len()
            );
        }
        let msg_type = u16::from_be_bytes([bytes[0], bytes[1]]);
        if msg_type & 0xC000 != 0 {
            bail!("STUN message type 最高两位必须为 0（0x{msg_type:04x}）");
        }
        let body_len = u16::from_be_bytes([bytes[2], bytes[3]]) as usize;
        if body_len % 4 != 0 {
            bail!("STUN 消息体长度未按 4 字节对齐：{body_len}");
        }
        let cookie = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        if cookie != MAGIC_COOKIE {
            bail!("STUN magic cookie 不匹配（0x{cookie:08x}）");
        }
        let end = HEADER_LEN
            .checked_add(body_len)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| {
                anyhow!(
                    "STUN 消息体越界：头部声明 {body_len} 字节，实际只有 {} 字节",
                    bytes.len() - HEADER_LEN
                )
            })?;
        if end != bytes.len() {
            bail!(
                "STUN 消息尾部多出 {} 字节（长度字段 {body_len}）",
                bytes.len() - end
            );
        }

        let mut transaction_id = [0u8; TRANSACTION_ID_LEN];
        transaction_id.copy_from_slice(&bytes[8..HEADER_LEN]);

        let mut attributes = Vec::new();
        let mut offset = HEADER_LEN;
        while offset < end {
            if end - offset < 4 {
                bail!(
                    "STUN 属性头越界（偏移 {offset}，剩余 {} 字节）",
                    end - offset
                );
            }
            let typ = u16::from_be_bytes([bytes[offset], bytes[offset + 1]]);
            let value_len = u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]]) as usize;
            let padded_len = (value_len + 3) & !3;
            let value_start = offset + 4;
            let value_end = value_start + value_len;
            if value_end > end || value_start + padded_len > end {
                bail!("STUN 属性（type=0x{typ:04x}）越界：值长 {value_len}，偏移 {offset}");
            }
            attributes.push(RawAttribute {
                typ,
                value: bytes[value_start..value_end].to_vec(),
            });
            offset = value_start + padded_len;
        }

        Ok(Self {
            msg_type,
            transaction_id,
            attributes,
        })
    }
}

/// `XOR-MAPPED-ADDRESS` 属性（RFC 5389 §15.2）。
pub fn xor_mapped_address_attribute(
    addr: SocketAddr,
    transaction_id: &[u8; TRANSACTION_ID_LEN],
) -> RawAttribute {
    RawAttribute::new(
        ATTR_XOR_MAPPED_ADDRESS,
        encode_address(addr, true, transaction_id),
    )
}

/// `MAPPED-ADDRESS` 属性（RFC 3489 兼容）。
pub fn mapped_address_attribute(addr: SocketAddr) -> RawAttribute {
    RawAttribute::new(ATTR_MAPPED_ADDRESS, encode_address(addr, false, &[0u8; 12]))
}

/// `RESPONSE-ORIGIN` 属性：STUN 服务器告诉客户端"这个响应实际从哪发出"。
pub fn response_origin_attribute(addr: SocketAddr) -> RawAttribute {
    RawAttribute::new(
        ATTR_RESPONSE_ORIGIN,
        encode_address(addr, false, &[0u8; 12]),
    )
}

/// `OTHER-ADDRESS` 属性：STUN 服务器的备用地址（用于 NAT 行为探测）。
pub fn other_address_attribute(addr: SocketAddr) -> RawAttribute {
    RawAttribute::new(ATTR_OTHER_ADDRESS, encode_address(addr, false, &[0u8; 12]))
}

/// `CHANGE-REQUEST` 属性（RFC 5780）。
pub fn change_request_attribute(change_ip: bool, change_port: bool) -> RawAttribute {
    let bits = ChangeFlags {
        change_ip,
        change_port,
    }
    .to_bits()
    .unwrap_or(0);
    RawAttribute::new(ATTR_CHANGE_REQUEST, bits.to_be_bytes().to_vec())
}

/// `ERROR-CODE` 属性（3 位类别 + 8 位编号）。
pub fn error_code_attribute(code: u16, reason: &str) -> RawAttribute {
    let class = ((code / 100) & 0x07) as u8;
    let number = (code % 100) as u8;
    let mut value = vec![0u8, 0u8, class, number];
    value.extend_from_slice(reason.as_bytes());
    RawAttribute::new(ATTR_ERROR_CODE, value)
}

/// 编码地址：普通（MAPPED/ORIGIN/OTHER）或 XOR 变体（IPv4 与 cookie 异或；IPv6 再叠事务 ID）。
fn encode_address(
    addr: SocketAddr,
    xor: bool,
    transaction_id: &[u8; TRANSACTION_ID_LEN],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(if addr.is_ipv4() { 8 } else { 20 });
    out.push(0); // 保留字节
    match addr {
        SocketAddr::V4(v4) => {
            out.push(0x01);
            let port = if xor {
                v4.port() ^ (MAGIC_COOKIE >> 16) as u16
            } else {
                v4.port()
            };
            out.extend_from_slice(&port.to_be_bytes());
            let mut ip = u32::from(*v4.ip());
            if xor {
                ip ^= MAGIC_COOKIE;
            }
            out.extend_from_slice(&ip.to_be_bytes());
        }
        SocketAddr::V6(v6) => {
            out.push(0x02);
            let port = if xor {
                v6.port() ^ (MAGIC_COOKIE >> 16) as u16
            } else {
                v6.port()
            };
            out.extend_from_slice(&port.to_be_bytes());
            let mut octets = v6.ip().octets();
            if xor {
                let cookie = MAGIC_COOKIE.to_be_bytes();
                for (byte, key) in octets
                    .iter_mut()
                    .zip(cookie.iter().chain(transaction_id.iter()))
                {
                    *byte ^= key;
                }
            }
            out.extend_from_slice(&octets);
        }
    }
    out
}

/// 解码地址属性值。
fn decode_address(
    value: &[u8],
    xor: bool,
    transaction_id: &[u8; TRANSACTION_ID_LEN],
) -> Result<SocketAddr> {
    if value.len() < 4 {
        bail!("STUN 地址属性过短：{} 字节", value.len());
    }
    let family = value[1];
    let mut port = u16::from_be_bytes([value[2], value[3]]);
    if xor {
        port ^= (MAGIC_COOKIE >> 16) as u16;
    }
    match family {
        0x01 => {
            if value.len() != 8 {
                bail!("IPv4 地址属性长度应为 8，实际 {}", value.len());
            }
            let mut raw = [0u8; 4];
            raw.copy_from_slice(&value[4..8]);
            let mut ip = u32::from_be_bytes(raw);
            if xor {
                ip ^= MAGIC_COOKIE;
            }
            Ok(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from(ip), port)))
        }
        0x02 => {
            if value.len() != 20 {
                bail!("IPv6 地址属性长度应为 20，实际 {}", value.len());
            }
            let mut raw = [0u8; 16];
            raw.copy_from_slice(&value[4..20]);
            if xor {
                let cookie = MAGIC_COOKIE.to_be_bytes();
                for (byte, key) in raw
                    .iter_mut()
                    .zip(cookie.iter().chain(transaction_id.iter()))
                {
                    *byte ^= key;
                }
            }
            Ok(SocketAddr::V6(SocketAddrV6::new(
                Ipv6Addr::from(raw),
                port,
                0,
                0,
            )))
        }
        other => bail!("未知 STUN 地址族 0x{other:02x}"),
    }
}

/// 生成随机 96 bit 事务 ID；最高位清零（RFC 5389 §6 要求请求事务 ID 的高位为 0）。
pub fn random_transaction_id() -> [u8; TRANSACTION_ID_LEN] {
    use rand::RngCore;
    let mut transaction_id = [0u8; TRANSACTION_ID_LEN];
    rand::thread_rng().fill_bytes(&mut transaction_id);
    transaction_id[0] &= 0x7f;
    transaction_id
}

/// STUN 探测结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StunResult {
    /// STUN 服务器观察到的本地映射地址（`XOR-MAPPED-ADDRESS`）。
    pub mapped: SocketAddr,
    /// 服务器声明的备用地址（`OTHER-ADDRESS`），用于后续 NAT 行为探测。
    pub other: Option<SocketAddr>,
    /// 实际回包的源地址；配合 [`stun_probe_with_change`] 可观察 NAT 对"换 IP/换端口回包"的过滤。
    pub source: SocketAddr,
    /// 从首次发送到收到响应的往返耗时。
    pub rtt: Duration,
}

/// 探测 STUN 服务器：绑定临时 IPv4/IPv6 socket 发 Binding Request，返回映射地址与 RTT。
///
/// 超时重传：最多 [`MAX_STUN_ATTEMPTS`] 次发送，间隔取 `timeout / MAX_STUN_ATTEMPTS`
/// （不低于 [`MIN_STUN_RETRANSMIT`]），整体受 `timeout` 约束。
pub async fn stun_probe(server: SocketAddr, timeout: Duration) -> Result<StunResult> {
    let socket = bind_probe_socket(server)?;
    probe_with_std_socket(socket, server, timeout, ChangeFlags::default()).await
}

/// 带 `CHANGE-REQUEST` 的探测（NAT 过滤行为判定用，见 `design/023` §5）。
pub async fn stun_probe_with_change(
    server: SocketAddr,
    timeout: Duration,
    change: ChangeFlags,
) -> Result<StunResult> {
    let socket = bind_probe_socket(server)?;
    probe_with_std_socket(socket, server, timeout, change).await
}

/// 在调用方提供的 socket 上探测（`try_clone` 同一 UDP 端口，不消耗原 socket）。
///
/// **注意**：探测会把底层 socket 置为非阻塞（打洞与 quinn 复用都需要），
/// 返回后原 socket 也保持非阻塞。
pub async fn stun_probe_on_socket(
    socket: &UdpSocket,
    server: SocketAddr,
    timeout: Duration,
) -> Result<StunResult> {
    stun_probe_on_socket_with_change(socket, server, timeout, ChangeFlags::default()).await
}

/// 在调用方 socket 上带 `CHANGE-REQUEST` 探测。
pub async fn stun_probe_on_socket_with_change(
    socket: &UdpSocket,
    server: SocketAddr,
    timeout: Duration,
    change: ChangeFlags,
) -> Result<StunResult> {
    let cloned = socket
        .try_clone()
        .context("复制本地 UDP socket 用于 STUN 探测失败")?;
    probe_with_std_socket(cloned, server, timeout, change).await
}

fn bind_probe_socket(server: SocketAddr) -> Result<UdpSocket> {
    let bind_addr = if server.is_ipv4() {
        SocketAddr::from(([0u8, 0, 0, 0], 0))
    } else {
        SocketAddr::from(([0u16; 8], 0))
    };
    UdpSocket::bind(bind_addr).with_context(|| format!("绑定 STUN 探测 socket（{bind_addr}）失败"))
}

async fn probe_with_std_socket(
    socket: UdpSocket,
    server: SocketAddr,
    timeout: Duration,
    change: ChangeFlags,
) -> Result<StunResult> {
    if timeout.is_zero() {
        bail!("STUN 探测超时必须大于 0");
    }
    socket
        .set_nonblocking(true)
        .context("把 STUN 探测 socket 设为非阻塞失败")?;
    let socket = TokioUdpSocket::from_std(socket).context("包装 STUN 探测 socket 失败")?;
    probe(&socket, server, timeout, change).await
}

async fn probe(
    socket: &TokioUdpSocket,
    server: SocketAddr,
    timeout: Duration,
    change: ChangeFlags,
) -> Result<StunResult> {
    let started = Instant::now();
    let deadline = started + timeout;
    let per_attempt = (timeout / MAX_STUN_ATTEMPTS)
        .max(MIN_STUN_RETRANSMIT)
        .min(timeout);
    let mut buffer = [0u8; 1500];
    let mut attempts = 0u32;
    let mut last_error: Option<anyhow::Error> = None;

    while attempts < MAX_STUN_ATTEMPTS && Instant::now() < deadline {
        attempts += 1;
        let transaction_id = random_transaction_id();
        let mut request = StunMessage::binding_request(transaction_id);
        if let Some(bits) = change.to_bits() {
            request = request.with_attribute(RawAttribute::new(
                ATTR_CHANGE_REQUEST,
                bits.to_be_bytes().to_vec(),
            ));
        }
        socket
            .send_to(&request.encode(), server)
            .await
            .with_context(|| {
                format!("向 STUN 服务器 {server} 发送 Binding Request 失败（第 {attempts} 次）")
            })?;
        trace!(%server, attempt = attempts, change = ?change, "已发送 STUN Binding Request");

        let attempt_deadline = (Instant::now() + per_attempt).min(deadline);
        loop {
            let remaining = attempt_deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                last_error = Some(anyhow!(
                    "等待 STUN 响应超时（{server}，第 {attempts} 次，{per_attempt:?}）"
                ));
                break;
            }
            match tokio::time::timeout(remaining, socket.recv_from(&mut buffer)).await {
                Ok(Ok((len, source))) => {
                    let now = Instant::now();
                    let response = match StunMessage::decode(&buffer[..len]) {
                        Ok(response) => response,
                        Err(error) => {
                            trace!(%source, %error, "忽略无法解析的 STUN 包");
                            continue;
                        }
                    };
                    if response.transaction_id != transaction_id {
                        trace!(%source, "忽略事务 ID 不匹配的 STUN 包");
                        continue;
                    }
                    if response.msg_type == BINDING_ERROR_RESPONSE {
                        let (code, reason) = response.error_code().unwrap_or((0, String::new()));
                        bail!("STUN 服务器 {server} 返回错误 {code} {reason}");
                    }
                    if response.msg_type != BINDING_SUCCESS_RESPONSE {
                        trace!(
                            %source,
                            msg_type = format_args!("0x{:04x}", response.msg_type),
                            "忽略非 Binding Success 的 STUN 响应"
                        );
                        continue;
                    }
                    let mapped = response
                        .xor_mapped_address()?
                        .or(response.mapped_address()?)
                        .ok_or_else(|| {
                            anyhow!("STUN 响应缺少 XOR-MAPPED-ADDRESS / MAPPED-ADDRESS（{server}）")
                        })?;
                    let other = response.other_address()?;
                    let result = StunResult {
                        mapped,
                        other,
                        source,
                        rtt: now - started,
                    };
                    debug!(
                        %server,
                        mapped = %mapped,
                        source = %source,
                        rtt_ms = result.rtt.as_secs_f64() * 1000.0,
                        attempt = attempts,
                        "STUN 探测成功"
                    );
                    return Ok(result);
                }
                Ok(Err(error)) => {
                    // 非阻塞 socket 的 WouldBlock、Windows 上 UDP 收到 ICMP 端口不可达后的
                    // ConnectionReset 等都属于瞬时错误：记账不必中断，稍等再试。
                    trace!(%server, %error, "接收 STUN 响应遇到瞬时错误，继续重试");
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                Err(_) => {
                    last_error = Some(anyhow!(
                        "等待 STUN 响应超时（{server}，第 {attempts} 次，{per_attempt:?}）"
                    ));
                    break;
                }
            }
        }
    }

    Err(last_error
        .unwrap_or_else(|| anyhow!("STUN 探测失败：{server} 未响应（共发送 {attempts} 次）")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn message_type_roundtrip_all_classes() {
        for class in [
            MessageClass::Request,
            MessageClass::Indication,
            MessageClass::SuccessResponse,
            MessageClass::ErrorResponse,
        ] {
            let encoded = encode_message_type(BINDING_METHOD, class);
            let (method, decoded) = decode_message_type(encoded);
            assert_eq!(method, BINDING_METHOD, "class {class:?} 方法号丢失");
            assert_eq!(decoded, class, "class {class:?} 解码不一致");
        }
        assert_eq!(
            BINDING_REQUEST,
            encode_message_type(BINDING_METHOD, MessageClass::Request)
        );
        assert_eq!(BINDING_SUCCESS_RESPONSE, 0x0101);
        assert_eq!(BINDING_ERROR_RESPONSE, 0x0111);
    }

    #[test]
    fn xor_mapped_address_roundtrip_v4() {
        let txid = [0x11u8; 12];
        let addr: SocketAddr = "192.0.2.33:54321".parse().unwrap();
        let msg = StunMessage::binding_success(txid, addr, None);
        let encoded = msg.encode();
        let decoded = StunMessage::decode(&encoded).expect("解码成功");
        assert_eq!(decoded.transaction_id, txid);
        assert_eq!(decoded.xor_mapped_address().unwrap(), Some(addr));
        // 线格式里不能出现明文 IP（"192.0.2.33" = c0 00 02 21），确认真的做了 XOR。
        assert!(!encoded.windows(4).any(|w| w == [192, 0, 2, 33]));
    }

    #[test]
    fn xor_mapped_address_roundtrip_v6() {
        let txid = [0x5Au8; 12];
        let addr: SocketAddr =
            SocketAddr::from((Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 1, 2, 3, 4), 8080));
        let msg = StunMessage::binding_success(txid, addr, None);
        let decoded = StunMessage::decode(&msg.encode()).expect("解码成功");
        assert_eq!(decoded.xor_mapped_address().unwrap(), Some(addr));
    }

    #[test]
    fn other_address_and_response_origin_roundtrip() {
        let txid = [0x22u8; 12];
        let mapped: SocketAddr = SocketAddr::from((Ipv4Addr::new(10, 0, 0, 5), 40000));
        let other: SocketAddr = SocketAddr::from((Ipv4Addr::new(198, 51, 100, 7), 3479));
        let origin = SocketAddr::from((Ipv4Addr::new(198, 51, 100, 8), 3478));
        let msg = StunMessage::binding_success(txid, mapped, Some(other))
            .with_attribute(response_origin_attribute(origin));
        let decoded = StunMessage::decode(&msg.encode()).expect("解码成功");
        assert_eq!(decoded.mapped_address().unwrap(), None);
        assert_eq!(decoded.xor_mapped_address().unwrap(), Some(mapped));
        assert_eq!(decoded.other_address().unwrap(), Some(other));
        assert_eq!(decoded.response_origin().unwrap(), Some(origin));
    }

    #[test]
    fn change_request_roundtrip() {
        let msg = StunMessage::binding_request([0u8; 12])
            .with_attribute(change_request_attribute(true, false));
        let decoded = StunMessage::decode(&msg.encode()).expect("解码成功");
        assert_eq!(decoded.change_request(), Some(CHANGE_IP_FLAG));
        assert_eq!(
            ChangeFlags {
                change_ip: false,
                change_port: true
            }
            .to_bits(),
            Some(CHANGE_PORT_FLAG)
        );
        assert_eq!(ChangeFlags::default().to_bits(), None);
    }

    #[test]
    fn message_integrity_is_preserved_as_raw_attribute() {
        let fake = [0xEEu8; 20];
        let msg = StunMessage::binding_request([0u8; 12])
            .with_attribute(RawAttribute::new(ATTR_MESSAGE_INTEGRITY, fake.to_vec()));
        let decoded = StunMessage::decode(&msg.encode()).expect("解码成功");
        assert_eq!(decoded.message_integrity(), Some(&fake[..]));
    }

    #[test]
    fn error_code_roundtrip() {
        let msg = StunMessage::binding_error([0x33u8; 12], 420, "Unknown Attribute");
        let decoded = StunMessage::decode(&msg.encode()).expect("解码成功");
        assert!(decoded.msg_type == BINDING_ERROR_RESPONSE);
        assert_eq!(
            decoded.error_code(),
            Some((420, "Unknown Attribute".to_string()))
        );
    }

    #[test]
    fn decode_rejects_malformed_headers() {
        let txid = [0x44u8; 12];
        let good = StunMessage::binding_request(txid).encode();

        assert!(
            StunMessage::decode(&good[..HEADER_LEN - 1]).is_err(),
            "过短"
        );

        let mut bad_cookie = good.clone();
        bad_cookie[4] ^= 0xFF;
        assert!(
            StunMessage::decode(&bad_cookie).is_err(),
            "magic cookie 错误"
        );

        let mut bad_length = good.clone();
        bad_length[2..4].copy_from_slice(&12345u16.to_be_bytes());
        assert!(StunMessage::decode(&bad_length).is_err(), "长度越界");

        let mut unaligned = good.clone();
        unaligned[2..4].copy_from_slice(&2u16.to_be_bytes());
        unaligned.extend_from_slice(&[0u8; 2]);
        assert!(StunMessage::decode(&unaligned).is_err(), "长度未对齐");

        let mut trailing = good.clone();
        trailing.push(0);
        assert!(StunMessage::decode(&trailing).is_err(), "尾部多余字节");
    }

    #[test]
    fn decode_rejects_truncated_attribute() {
        let txid = [0x55u8; 12];
        let mut encoded = StunMessage::binding_request(txid)
            .with_attribute(RawAttribute::new(ATTR_SOFTWARE, b"dfy".to_vec()))
            .encode();
        // 声明一个 8 字节值但只留 3 字节实体。
        encoded[HEADER_LEN + 2..HEADER_LEN + 4].copy_from_slice(&8u16.to_be_bytes());
        assert!(StunMessage::decode(&encoded).is_err());
    }

    #[test]
    fn random_transaction_id_is_well_formed() {
        let a = random_transaction_id();
        let b = random_transaction_id();
        assert_eq!(a[0] & 0x80, 0, "事务 ID 最高位必须为 0");
        assert_ne!(a, b, "两次随机事务 ID 不应相同（概率可忽略不计）");
    }

    #[test]
    fn unknown_attribute_is_kept() {
        let msg = StunMessage::binding_request([0u8; 12])
            .with_attribute(RawAttribute::new(0xC0DE, vec![1, 2, 3, 4, 5]));
        let decoded = StunMessage::decode(&msg.encode()).expect("解码成功");
        assert_eq!(decoded.attribute(0xC0DE), Some(&[1, 2, 3, 4, 5][..]));
    }
}
