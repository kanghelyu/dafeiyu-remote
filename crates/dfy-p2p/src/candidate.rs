// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 候选枚举与优先级（RFC 8445 §5.1.2 的 priority 公式，做 P2P 场景裁剪）。
//!
//! 三类候选：
//! - `Host`：本机 UDP socket 的直连地址（局域网内对端可直接使用）；
//! - `ServerReflexive`：由 STUN 得到的 NAT 公网映射地址；
//! - `Relay`：中继会合点地址（P2P 失败时的兜底，由 `dfy-relay` 侧提供）。
//!
//! ## 本机网卡枚举的范围（已知局限）
//!
//! 不引入 `windows` crate（避免把 Win32 重依赖带进传输层）。Host 候选用纯 std 的
//! "UDP connect 外部地址但不发包"技巧拿默认路由出口 IP：UDP `connect` 只在本地路由表里
//! 选源地址，不会发出任何报文。它只能发现**主出口网卡的一个 IPv4 地址**；
//! 多网卡 / 多出口 / IPv6 场景请由信令层补充候选，或直接用 STUN 得到的 srflx。
//! 完全无网络（无默认路由）时返回空列表，调用方应跳过 Host 候选、仅用 srflx/relay。
//!
//! 详细设计与待办见 `design/023-p2p-punch.md` §3、§8。

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::{debug, warn};

use crate::stun::{stun_probe_on_socket, DEFAULT_STUN_TIMEOUT};

/// 默认本地优先级（RFC 8445 的 local preference，取值越大越优先）。
pub const DEFAULT_LOCAL_PREFERENCE: u16 = 65535;
/// 默认分量 ID：P2P 只承载一条 QUIC 连接，固定为 RFC 8445 的 RTP 分量 1。
pub const DEFAULT_COMPONENT_ID: u16 = 1;

/// 候选类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CandidateKind {
    /// 本机直连地址（同一局域网 / 回环自测）。
    Host,
    /// STUN 得到的服务器反射地址（NAT 公网映射）。
    ServerReflexive,
    /// 中继会合地址（兜底路径）。
    Relay,
}

impl CandidateKind {
    /// RFC 8445 的类型优先级：Host 126 > srflx 100 > relay 0。
    pub const fn type_preference(self) -> u32 {
        match self {
            CandidateKind::Host => 126,
            CandidateKind::ServerReflexive => 100,
            CandidateKind::Relay => 0,
        }
    }

    /// 稳定短名（日志 / 信令 JSON 用）。
    pub const fn as_str(self) -> &'static str {
        match self {
            CandidateKind::Host => "host",
            CandidateKind::ServerReflexive => "srflx",
            CandidateKind::Relay => "relay",
        }
    }
}

impl std::fmt::Display for CandidateKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一个候选地址：类型 + 地址 + 优先级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Candidate {
    /// 候选类型。
    pub kind: CandidateKind,
    /// UDP 地址。
    pub addr: SocketAddr,
    /// 优先级（越大越优先），由 [`candidate_priority`] 计算。
    pub priority: u32,
}

impl Candidate {
    /// 用默认本地优先级与分量 ID 计算优先级。
    pub fn new(kind: CandidateKind, addr: SocketAddr) -> Self {
        Self {
            kind,
            addr,
            priority: candidate_priority(kind, DEFAULT_LOCAL_PREFERENCE, DEFAULT_COMPONENT_ID),
        }
    }

    /// 自定义本地优先级 / 分量 ID 构造（例如按网卡测速调权）。
    pub fn with_preference(kind: CandidateKind, addr: SocketAddr, local_preference: u16) -> Self {
        Self {
            kind,
            addr,
            priority: candidate_priority(kind, local_preference, DEFAULT_COMPONENT_ID),
        }
    }

    /// 中继候选（type preference 为 0，优先级自然最低）。
    pub fn relay(addr: SocketAddr) -> Self {
        Self::new(CandidateKind::Relay, addr)
    }
}

impl std::fmt::Display for Candidate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}={}（priority={}）",
            self.kind, self.addr, self.priority
        )
    }
}

/// RFC 8445 §5.1.2：`priority = 2^24 × type_pref + 2^8 × local_pref + (256 - component_id)`。
pub const fn candidate_priority(
    kind: CandidateKind,
    local_preference: u16,
    component_id: u16,
) -> u32 {
    (kind.type_preference() << 24) + ((local_preference as u32) << 8) + (256 - component_id as u32)
}

/// 按优先级降序排序；优先级相同按类型（Host > srflx > relay）再按地址稳定排序。
pub fn sort_candidates(candidates: &mut [Candidate]) {
    candidates.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then_with(|| b.kind.type_preference().cmp(&a.kind.type_preference()))
            .then_with(|| a.addr.cmp(&b.addr))
    });
}

/// 去重（同一 `addr` 保留优先级最高的一条），并返回降序排序后的列表。
pub fn dedup_and_sort(candidates: impl IntoIterator<Item = Candidate>) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    for candidate in candidates {
        match out
            .iter_mut()
            .find(|existing| existing.addr == candidate.addr)
        {
            Some(existing) if candidate.priority > existing.priority => *existing = candidate,
            Some(_) => {}
            None => out.push(candidate),
        }
    }
    sort_candidates(&mut out);
    out
}

/// 枚举本机 Host 候选（仅 IPv4 主出口地址，见模块头部的局限说明）。
///
/// `local_port` 是随后打洞/QUIC 监听用的端口；返回地址的端口与之一致。
/// 无默认路由（离线机器）时返回空列表。
pub fn gather_local_candidates(local_port: u16) -> Vec<SocketAddr> {
    match primary_local_ipv4() {
        Some(ip) => vec![SocketAddr::new(IpAddr::V4(ip), local_port)],
        None => {
            debug!("未发现默认路由出口网卡，Host 候选为空（仅依赖 STUN srflx）");
            Vec::new()
        }
    }
}

/// 纯 std 的默认出口 IPv4 探测：UDP `connect` 只选源地址，不发送任何报文。
fn primary_local_ipv4() -> Option<Ipv4Addr> {
    // 依次尝试常见外部地址；UDP connect 在本地路由表命中即可，无需真的可达。
    const PROBES: [&str; 3] = ["8.8.8.8:80", "1.1.1.1:53", "192.168.1.1:80"];
    for probe in PROBES {
        let Ok(dest) = probe.parse::<SocketAddr>() else {
            continue;
        };
        let Ok(socket) = UdpSocket::bind(SocketAddr::from(([0u8, 0, 0, 0], 0))) else {
            continue;
        };
        if socket.connect(dest).is_err() {
            continue;
        }
        if let Ok(local) = socket.local_addr() {
            if let IpAddr::V4(ip) = local.ip() {
                if !ip.is_unspecified() && !ip.is_loopback() {
                    return Some(ip);
                }
            }
        }
    }
    None
}

/// NAT 映射一致性提示（用多个 STUN 服务器对同一 socket 探测得到）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NatHint {
    /// 没有 STUN 服务器，或只探测成功一个，无法判断。
    #[default]
    Unknown,
    /// 多个 STUN 服务器看到同一映射地址 → 映射与目的无关（full/restricted cone 倾向）。
    ConsistentMapping,
    /// 多个 STUN 服务器看到不同映射地址 → 疑似对称 NAT，应优先 relay。
    InconsistentMapping,
}

/// 候选收集结果：候选列表 + 已绑定的 UDP socket（打洞必须复用同一 socket/端口）。
#[derive(Debug)]
pub struct GatheredCandidates {
    /// 已去重排序的候选（Host + srflx；relay 由调用方追加）。
    pub candidates: Vec<Candidate>,
    /// 绑定在 `local_port` 的 socket；打洞时交给 [`crate::punch`]。
    pub socket: UdpSocket,
    /// 实际本地端口（`local_port = 0` 时由内核分配）。
    pub local_port: u16,
    /// NAT 映射一致性提示。
    pub nat_hint: NatHint,
}

impl GatheredCandidates {
    /// 是否建议直接走 relay（疑似对称 NAT 且没有任何可打洞候选）。
    pub fn suggests_relay(&self) -> bool {
        self.nat_hint == NatHint::InconsistentMapping
            || self
                .candidates
                .iter()
                .all(|candidate| candidate.kind == CandidateKind::Relay)
    }
}

/// 收集候选但不保留 socket（需要自己另外绑定端口时用；否则优先 [`gather_with_socket`]）。
pub async fn gather_candidates(
    stun_servers: &[SocketAddr],
    local_port: u16,
    stun_timeout: Duration,
) -> Result<Vec<Candidate>> {
    Ok(gather_with_socket(stun_servers, local_port, stun_timeout)
        .await?
        .candidates)
}

/// 收集候选：绑定 `0.0.0.0:local_port` → 本机 Host 候选 → 逐台 STUN 探测 srflx。
///
/// 打洞必须复用 STUN 拿映射所用的**同一个 socket/端口**，所以这里把 socket 一并返回。
/// 所有 STUN 服务器失败时不是硬错误：返回空/仅 Host 候选，由调用方决定回退 relay。
pub async fn gather_with_socket(
    stun_servers: &[SocketAddr],
    local_port: u16,
    stun_timeout: Duration,
) -> Result<GatheredCandidates> {
    if stun_timeout.is_zero() {
        anyhow::bail!("STUN 收集超时必须大于 0");
    }
    let bind_addr = SocketAddr::from(([0u8, 0, 0, 0], local_port));
    let socket = UdpSocket::bind(bind_addr)
        .with_context(|| format!("绑定 P2P 本地 UDP socket（{bind_addr}）失败"))?;
    socket
        .set_nonblocking(true)
        .context("把 P2P 本地 UDP socket 设为非阻塞失败")?;
    let local_port = socket
        .local_addr()
        .context("读取 P2P 本地 UDP socket 地址失败")?
        .port();

    let mut candidates: Vec<Candidate> = gather_local_candidates(local_port)
        .into_iter()
        .map(|addr| Candidate::new(CandidateKind::Host, addr))
        .collect();

    let mut mapped_addrs: Vec<SocketAddr> = Vec::new();
    let mut successful_probes = 0u32;
    for server in stun_servers {
        match stun_probe_on_socket(&socket, *server, stun_timeout).await {
            Ok(result) => {
                successful_probes += 1;
                debug!(%server, mapped = %result.mapped, rtt_ms = result.rtt.as_secs_f64() * 1000.0, "STUN 收集到 srflx 候选");
                if !result.mapped.ip().is_unspecified() && !mapped_addrs.contains(&result.mapped) {
                    mapped_addrs.push(result.mapped);
                    candidates.push(Candidate::new(
                        CandidateKind::ServerReflexive,
                        result.mapped,
                    ));
                }
            }
            Err(error) => {
                warn!(%server, %error, "STUN 探测失败，跳过该服务器（可能回退 relay）");
            }
        }
    }

    // 一个服务器无法判断映射行为；多个服务器看到同一映射 = 与目的无关（cone 倾向），
    // 看到不同映射 = 疑似对称 NAT（srflx 对打洞无效，应优先 relay）。
    let nat_hint = if successful_probes < 2 {
        NatHint::Unknown
    } else if mapped_addrs.len() == 1 {
        NatHint::ConsistentMapping
    } else {
        NatHint::InconsistentMapping
    };

    if candidates.is_empty() {
        warn!("没有收集到任何 Host/srflx 候选：请检查网络或直接使用 relay 路径");
    }

    Ok(GatheredCandidates {
        candidates: dedup_and_sort(candidates),
        socket,
        local_port,
        nat_hint,
    })
}

/// 便捷封装：绑定 IPv4 UDP socket（`port = 0` 由内核分配）。
pub fn bind_udp_socket(port: u16) -> Result<UdpSocket> {
    let addr = SocketAddr::from(([0u8, 0, 0, 0], port));
    let socket = UdpSocket::bind(addr).with_context(|| format!("绑定 UDP socket（{addr}）失败"))?;
    socket
        .set_nonblocking(true)
        .context("把 UDP socket 设为非阻塞失败")?;
    Ok(socket)
}

/// 默认的候选收集超时（与一次 STUN 探测相同）。
pub const DEFAULT_GATHER_TIMEOUT: Duration = DEFAULT_STUN_TIMEOUT;

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddrV4;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 20), port))
    }

    #[test]
    fn priority_formula_matches_rfc8445_defaults() {
        let host = candidate_priority(CandidateKind::Host, 65535, 1);
        let srflx = candidate_priority(CandidateKind::ServerReflexive, 65535, 1);
        let relay = candidate_priority(CandidateKind::Relay, 65535, 1);
        assert_eq!(host, 0x7EFF_FFFF);
        assert_eq!(srflx, 0x64FF_FFFF);
        assert_eq!(relay, 0x00FF_FFFF);
        assert!(host > srflx && srflx > relay);
    }

    #[test]
    fn candidate_new_uses_default_priority() {
        let candidate = Candidate::new(CandidateKind::Host, addr(40000));
        assert_eq!(
            candidate.priority,
            candidate_priority(CandidateKind::Host, 65535, 1)
        );
        assert_eq!(Candidate::relay(addr(40000)).kind, CandidateKind::Relay);
        assert_eq!(Candidate::relay(addr(40000)).priority, 0x00FF_FFFF);
    }

    #[test]
    fn sort_orders_host_then_srflx_then_relay() {
        let mut candidates = vec![
            Candidate::relay(addr(1)),
            Candidate::new(CandidateKind::ServerReflexive, addr(2)),
            Candidate::new(CandidateKind::Host, addr(3)),
            Candidate::new(CandidateKind::ServerReflexive, addr(4)),
        ];
        sort_candidates(&mut candidates);
        assert_eq!(candidates[0].kind, CandidateKind::Host);
        assert_eq!(candidates[1].kind, CandidateKind::ServerReflexive);
        assert_eq!(candidates[2].kind, CandidateKind::ServerReflexive);
        assert_eq!(candidates[3].kind, CandidateKind::Relay);
        // 同类型按地址稳定排序
        assert!(candidates[1].addr < candidates[2].addr);
    }

    #[test]
    fn dedup_keeps_highest_priority_per_address() {
        let relay_dup = Candidate::relay(addr(5000));
        let host_dup = Candidate::new(CandidateKind::Host, addr(5000));
        let deduped = dedup_and_sort(vec![relay_dup, host_dup, Candidate::relay(addr(5001))]);
        assert_eq!(deduped.len(), 2);
        assert_eq!(deduped[0].kind, CandidateKind::Host);
        assert_eq!(deduped[0].addr, addr(5000));
    }

    #[test]
    fn gather_local_candidates_uses_requested_port() {
        let candidates = gather_local_candidates(41234);
        // 离线机器允许为空；有网卡时必须全是指定端口，且不能是 0.0.0.0 占位。
        assert!(candidates
            .iter()
            .all(|addr| addr.port() == 41234 && !addr.ip().is_unspecified()));
    }

    #[tokio::test]
    async fn gather_without_stun_servers_returns_socket_and_host_candidates() {
        let gathered = gather_with_socket(&[], 0, Duration::from_millis(200))
            .await
            .expect("绑定 socket 应成功");
        assert!(gathered.local_port > 0);
        assert_eq!(
            gathered.socket.local_addr().unwrap().port(),
            gathered.local_port
        );
        assert_eq!(gathered.nat_hint, NatHint::Unknown);
        // 候选里不应有 srflx（没有 STUN 服务器）。
        assert!(!gathered
            .candidates
            .iter()
            .any(|candidate| candidate.kind == CandidateKind::ServerReflexive));
    }

    #[test]
    fn suggests_relay_when_mapping_inconsistent_or_no_punchable_candidates() {
        let mut gathered = GatheredCandidates {
            candidates: vec![Candidate::relay(addr(9))],
            socket: bind_udp_socket(0).expect("绑定 socket 成功"),
            local_port: 0,
            nat_hint: NatHint::Unknown,
        };
        assert!(gathered.suggests_relay(), "只有 relay 候选时应建议走 relay");

        gathered
            .candidates
            .push(Candidate::new(CandidateKind::Host, addr(10)));
        assert!(
            !gathered.suggests_relay(),
            "有 Host 候选且映射一致可尝试打洞"
        );

        gathered.nat_hint = NatHint::InconsistentMapping;
        assert!(gathered.suggests_relay(), "疑似对称 NAT 应优先 relay");
    }
}
