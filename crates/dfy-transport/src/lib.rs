//! # dfy-transport — DFYP/1 的 QUIC 传输封装（大肥鱼远程）
//!
//! 在 [`dfy_proto`] 定义的线格式之上提供：
//! - [`tls`]：rcgen 自签身份 + SPKI 指纹校验（TOFU）、`quinn` 服务端/客户端配置；
//! - [`ctrl`]：control 双向流的分帧收/发（`send_ctrl` / `recv_ctrl`）；
//! - [`handshake`]：`Hello → Challenge → Auth → Welcome → CapabilityAck` 握手；
//! - [`session`]：[`session::AgentSession`] / [`session::ClientSession`] 与 5s/15s 心跳状态机；
//! - [`datagram`]：视频 DATAGRAM 分片发送、input-move 发送、接收统计；
//! - [`endpoint`]：Agent 监听 / Client 拨号。
//!
//! ```no_run
//! # async fn demo() -> anyhow::Result<()> {
//! use dfy_transport::{bind_agent, client_tls, connect, client_handshake};
//! let (server_ep, _identity, addr) = dfy_transport::bind_agent_random()?;
//! let tls = client_tls(None)?;
//! let (_client_ep, conn) = connect(addr, tls).await?;
//! let (welcome, _chan) = client_handshake(&conn, "test", "demo-client", vec![]).await?;
//! println!("session = {}", hex::encode(&welcome.session_id));
//! # server_ep.close(0u32.into(), b"done");
//! # Ok(()) }
//! ```
//!
//! 说明：MVP 简化点（口令派生、证书指纹固定策略、FEC/重传、完整 session 状态机）
//! 在 `spec/DFYP-v1.md` 与 `research/050-oss-base-and-arch.md` 中有记录。

#![forbid(unsafe_code)]

pub mod ctrl;
pub mod datagram;
pub mod endpoint;
pub mod handshake;
pub mod session;
pub mod tls;
pub mod util;

use std::time::Duration;

pub use ctrl::{CtrlChannel, CtrlReceiver, CtrlSender};
pub use datagram::{send_input_move, send_video_frame, VideoReceiver, VideoSendStats};
pub use endpoint::{bind_agent, bind_agent_random, connect};
pub use handshake::{client_handshake, initial_config_frames, server_handshake, ServerAuth};
pub use session::{AgentSession, ClientSession, Heartbeat, PING_INTERVAL, PING_TIMEOUT};
pub use tls::{client_tls, fingerprint_of_cert, server_config, ClientTls, TlsIdentity};
pub use util::{
    ct_eq, human_bytes, now_us, parse_fingerprint_hex, password_proof, random_bytes, sha256,
    sha256_hex, verify_proof,
};

/// 握手总超时（含 Challenge/Auth/Welcome/Ack 的往返）。
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
