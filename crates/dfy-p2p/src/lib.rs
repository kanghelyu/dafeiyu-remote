// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Dafeiyu Remote contributors

//! `dfy-p2p`：P2P 直连优先、中继兜底的打洞层（业主 §17）。
//!
//! 三个层次：
//! - [`stun`]：clean-room 的 RFC 5389 / RFC 5780 子集（Binding Request/Response、
//!   XOR-MAPPED-ADDRESS、CHANGE-REQUEST、RESPONSE-ORIGIN / OTHER-ADDRESS、超时重传）；
//! - [`candidate`]：候选枚举（host / srflx / relay）与 RFC 8445 优先级排序，
//!   以及多 STUN 服务器映射一致性（[`NatHint`]）判定；
//! - [`punch`]：双端并发打洞，成功后返回已连通的 UDP socket + 对端真实地址，
//!   直接交给 `dfy-transport` 的 quinn `Endpoint` 复用（见 `design/023-p2p-punch.md` §6）。
//!
//! 典型用法（信令层交换候选 → 双方同时打洞）：
//!
//! ```no_run
//! use std::net::SocketAddr;
//! use std::time::Duration;
//! use dfy_p2p::{gather_and_punch, PunchOptions};
//!
//! # async fn example(stun_servers: Vec<SocketAddr>, remote_candidates: Vec<SocketAddr>) -> anyhow::Result<()> {
//! let outcome = gather_and_punch(
//!     &stun_servers,
//!     &remote_candidates,
//!     0, // 由内核分配端口
//!     Duration::from_secs(2),
//!     PunchOptions::default(),
//! )
//! .await?;
//! // outcome.session.socket 可直接交给 quinn Endpoint；失败时回退 outcome.nat_hint / relay。
//! # Ok(())
//! # }
//! ```
//!
//! 真实公网 STUN / 跨 NAT 环境无法在单机自测中验证，回环测试覆盖见 `tests/p2p_loopback.rs`。

#![forbid(unsafe_code)]

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Result;

pub mod candidate;
pub mod punch;
pub mod stun;

pub use candidate::*;
pub use punch::*;
pub use stun::*;

/// 打洞流程的完整结果（收集 + 打洞一次完成）。
#[derive(Debug)]
pub struct PunchOutcome {
    /// 打洞成功的会话（socket + 对端真实地址）。
    pub session: P2PSession,
    /// 本地收集到的候选（host + srflx），可用于日志/诊断/信令上报。
    pub candidates: Vec<Candidate>,
    /// NAT 映射一致性提示；[`NatHint::InconsistentMapping`] 时本次打洞大概率无法成功。
    pub nat_hint: NatHint,
    /// 本地实际端口（`local_port = 0` 时为内核分配的端口）。
    pub local_port: u16,
}

/// 收集候选并保留打洞要用的 socket（门面函数）。
///
/// 与 [`candidate::gather_with_socket`] 相同，只是放在 crate 根方便调用。
pub async fn gather_for_punch(
    stun_servers: &[SocketAddr],
    local_port: u16,
    stun_timeout: Duration,
) -> Result<GatheredCandidates> {
    candidate::gather_with_socket(stun_servers, local_port, stun_timeout).await
}

/// 一步完成「收集本端候选 → 用同一 socket 向对端候选并发打洞」。
///
/// 失败（超时 / 无候选）时调用方应回退 `dfy-relay` 的中继路径；成功时
/// [`PunchOutcome::session`] 里的非阻塞 socket 可直接交给
/// `dfy_transport::agent_endpoint_on_socket` / `connect_on_socket`。
pub async fn gather_and_punch(
    stun_servers: &[SocketAddr],
    remote_candidates: &[SocketAddr],
    local_port: u16,
    stun_timeout: Duration,
    opts: PunchOptions,
) -> Result<PunchOutcome> {
    let gathered = gather_for_punch(stun_servers, local_port, stun_timeout).await?;
    let GatheredCandidates {
        candidates,
        socket,
        local_port,
        nat_hint,
    } = gathered;
    let session = punch::punch(socket, remote_candidates, opts).await?;
    Ok(PunchOutcome {
        session,
        candidates,
        nat_hint,
        local_port,
    })
}
