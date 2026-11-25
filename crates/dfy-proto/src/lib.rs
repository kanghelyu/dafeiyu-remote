//! # dfy-proto — DFYP/1 线格式（大肥鱼远程）
//!
//! clean-room 实现：所有消息、字段号、通道号、标志位均由 `spec/DFYP-v1.md` 独立推导，
//! **不复制**任何第三方协议的 schema 或常量表。
//!
//! 组成：
//! - [`msg`]：手写 prost 消息（`Envelope` + oneof），不依赖 protoc；
//! - [`frame`]：`control` 流帧 = 4B 大端长度 + protobuf，8 MiB 上限保护；
//! - [`datagram`]：视频 DATAGRAM 头（16B）与 input-move DATAGRAM 编解码；
//! - [`consts`]：版本 / ALPN / 通道号 / 错误码 / 权限位 / 输入类别。
//!
//! ```no_run
//! use dfy_proto::{encode_frame, decode_frame, Envelope};
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let frame = encode_frame(&Envelope::ping(42))?;
//! let msg = decode_frame(&frame)?;
//! assert_eq!(msg.kind_name(), "Ping");
//! # Ok(()) }
//! ```

#![forbid(unsafe_code)]

pub mod consts;
pub mod datagram;
pub mod frame;
pub mod msg;

pub use consts::*;
pub use datagram::{decode_input_move, decode_video, encode_input_move, encode_video, VideoHeader};
pub use frame::{decode_frame, encode_frame, encode_frame_into, peek_frame_len, FRAME_LEN_PREFIX};
pub use msg::{
    envelope, Auth, Bye, CapabilityAck, Challenge, ClipboardApplied, ClipboardDataChunk,
    ClipboardFormat, ClipboardFormatList, ClipboardPull, Envelope, Hello, InputEvent, Monitor,
    MonitorList, Ping, Pong, Reject, VideoConfig, Welcome,
};

use std::fmt;

/// `dfy-proto` 错误：编解码/边界校验失败。
#[derive(Debug)]
pub enum ProtoError {
    /// 帧/负载声明长度超过上限。
    Oversize { len: usize, max: usize },
    /// 长度前缀与可用字节不一致。
    LengthMismatch { declared: usize, available: usize },
    /// 缓冲区不足以容纳头部。
    ShortHeader { need: usize, got: usize },
    /// DATAGRAM 通道号不符合预期。
    BadChannel { got: u8 },
    /// 分片序号非法（`pkt_cnt == 0` 或 `pkt_idx >= pkt_cnt`）。
    BadPacketIndex { idx: u16, cnt: u16 },
    /// protobuf 解码失败。
    Decode(prost::DecodeError),
    /// protobuf 编码失败。
    Encode(prost::EncodeError),
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtoError::Oversize { len, max } => {
                write!(f, "帧过大: {len} 字节 > 上限 {max} 字节")
            }
            ProtoError::LengthMismatch {
                declared,
                available,
            } => {
                write!(
                    f,
                    "帧长度前缀声明 {declared} 字节，但缓冲区只剩 {available} 字节"
                )
            }
            ProtoError::ShortHeader { need, got } => {
                write!(f, "头部长度不足: 需要 {need} 字节，只有 {got} 字节")
            }
            ProtoError::BadChannel { got } => write!(f, "DATAGRAM 通道号非法: {got}"),
            ProtoError::BadPacketIndex { idx, cnt } => {
                write!(f, "视频分片序号非法: pkt_idx={idx}, pkt_cnt={cnt}")
            }
            ProtoError::Decode(e) => write!(f, "protobuf 解码失败: {e}"),
            ProtoError::Encode(e) => write!(f, "protobuf 编码失败: {e}"),
        }
    }
}

impl std::error::Error for ProtoError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ProtoError::Decode(e) => Some(e),
            ProtoError::Encode(e) => Some(e),
            _ => None,
        }
    }
}

impl From<prost::DecodeError> for ProtoError {
    fn from(value: prost::DecodeError) -> Self {
        ProtoError::Decode(value)
    }
}

impl From<prost::EncodeError> for ProtoError {
    fn from(value: prost::EncodeError) -> Self {
        ProtoError::Encode(value)
    }
}
