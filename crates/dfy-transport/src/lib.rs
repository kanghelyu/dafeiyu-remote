//! # dfy-transport — DFYP/1 的 QUIC 传输封装（大肥鱼远程）
//!
//! 在 [`dfy_proto`] 定义的线格式之上提供：
//! - [`tls`]：rcgen 自签身份 + 可选 SPKI 指纹固定、`quinn` 服务端/客户端配置；
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
//! 认证 v2：固定 Argon2id 参数、TLS exporter 绑定、双方 nonce 和角色分离的口令互证。
//! 这不是 PAKE；Android 被控端已持久化受 AndroidKeyStore 保护的 TLS 私钥，但产品级设备身份、配对、撤销、其它平台的 OS 密钥存储和完整 session 状态机仍待实现。
//! 证书 pin 策略、FEC/重传等边界在 `spec/DFYP-v1.md` 中记录。

#![forbid(unsafe_code)]

mod auth;

pub mod ctrl;
pub mod datagram;
pub mod endpoint;
pub mod handshake;
pub mod resume;
pub mod session;
pub mod tls;
pub mod util;

use std::time::Duration;

pub use ctrl::{CtrlChannel, CtrlReceiver, CtrlSender};
pub use datagram::{send_input_move, send_video_frame, VideoReceiver, VideoSendStats};
pub use endpoint::{
    agent_endpoint_on_socket, bind_agent, bind_agent_random, bind_agent_with_identity, connect,
    connect_on_socket,
};
pub use handshake::{
    client_handshake, client_handshake_context, client_handshake_context_for_session,
    client_resume_handshake, initial_config_frames, server_handshake, server_handshake_context,
    server_resume_request, ClientHandshake, ServerAuth, ServerHandshake,
};
pub use resume::{ResumeGrant, ResumeRegistry};
pub use session::{AgentSession, ClientSession, Heartbeat, PING_INTERVAL, PING_TIMEOUT};
pub use tls::{client_tls, fingerprint_of_cert, server_config, ClientTls, TlsIdentity};
pub use util::{
    ct_eq, human_bytes, now_us, parse_fingerprint_hex, random_bytes, sha256, sha256_hex,
};

/// 握手总超时（含 Challenge/Auth/Welcome/Ack 的往返）。
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
