// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Dafeiyu Remote contributors
// Copyright (c) 2026 Dafeiyu Remote contributors
//! tag 会话表：配对、转发、重绑定、回收、限速钩子。
//!
//! 并发模型：整个表由一把 `std::sync::Mutex` 保护（单 UDP recv 循环为主路径，
//! 每次临界区只做哈希查找 + 构造 egress，不 await、不 IO），因此锁竞争极小。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::mpsc;
use tracing::{debug, info, trace};

use crate::auth::{self, NonceCache};
use crate::config::Limits;
use crate::metrics::{DropReason, Metrics};
use crate::proto::{self, Tag, CHAN_CTRL, CHAN_DATA};
use crate::ratelimit::TokenBucket;

pub const ROLE_INIT: u8 = 0;
pub const ROLE_RESP: u8 = 1;

/// 统一取锁入口：本 crate 的锁只护短临界区（哈希查找 + 构造 egress，不 await 不做 IO），
/// 一旦中毒说明某任务 panic 过；继续使用内部值而不是整体崩溃。
pub fn lock(state: &std::sync::Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

/// 端点承载方式：UDP 地址或 TCP 连接（写任务 mpsc）。
#[derive(Clone)]
pub enum EndpointKind {
    Udp(SocketAddr),
    Tcp(mpsc::Sender<Vec<u8>>),
}

pub struct Endpoint {
    pub kind: EndpointKind,
    /// 唯一 id：TCP 重连/重绑定时用于让旧连接退出时不要误删新端点。
    pub id: u64,
    pub last_seen: Instant,
}

struct TagSession {
    #[allow(dead_code)]
    tag: Tag,
    ends: [Option<Endpoint>; 2],
    created: Instant,
    last_seen: Instant,
    /// `None` = 不限速。
    bucket: Option<TokenBucket>,
}

/// 转发动作：由数据面执行实际 IO。
pub struct Egress {
    pub kind: EgressKind,
}

pub enum EgressKind {
    Udp {
        to: SocketAddr,
        data: Vec<u8>,
    },
    Tcp {
        tx: mpsc::Sender<Vec<u8>>,
        frame: Vec<u8>,
    },
}

impl Egress {
    pub fn payload_len(&self) -> usize {
        match &self.kind {
            EgressKind::Udp { data, .. } => data.len(),
            EgressKind::Tcp { frame, .. } => frame.len().saturating_sub(5),
        }
    }

    fn udp(to: SocketAddr, data: Vec<u8>) -> Self {
        Self {
            kind: EgressKind::Udp { to, data },
        }
    }

    fn tcp(tx: mpsc::Sender<Vec<u8>>, frame: Vec<u8>) -> Self {
        Self {
            kind: EgressKind::Tcp { tx, frame },
        }
    }
}

pub enum BindOutcome {
    Ok {
        tag: Tag,
        role: u8,
        /// 端点 id（TCP 连接退出时用它避免误删重绑定后的新端点）。
        id: u64,
        reply: Vec<u8>,
        rebind: bool,
        expires_in_s: u32,
    },
    Reject {
        reply: Vec<u8>,
        code: u8,
    },
    /// 静默丢弃（不回任何包，防探测）。
    Silent,
}

pub struct State {
    limits: Limits,
    metrics: Arc<Metrics>,
    sessions: HashMap<Tag, TagSession>,
    /// 仅 UDP 端点：源地址 → (tag, role)。
    by_addr: HashMap<SocketAddr, (Tag, u8)>,
    nonces: NonceCache,
    global: Option<TokenBucket>,
    next_id: u64,
}

impl State {
    pub fn new(limits: Limits, metrics: Arc<Metrics>) -> Self {
        let nonces = NonceCache::new(
            limits.nonce_capacity,
            std::time::Duration::from_secs(limits.bind_window_s * 2 + 60),
        );
        let global = limits
            .global_bytes_per_s
            .map(|rate| TokenBucket::new(rate, (rate * 0.1).max(256.0 * 1024.0), Instant::now()));
        Self {
            limits,
            metrics,
            sessions: HashMap::new(),
            by_addr: HashMap::new(),
            nonces,
            global,
            next_id: 1,
        }
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// 处理 bind（UDP 传 `src`，TCP 传 `tcp_tx`）。
    pub fn bind(
        &mut self,
        key: &[u8],
        pkt: &[u8],
        src: Option<SocketAddr>,
        tcp_tx: Option<mpsc::Sender<Vec<u8>>>,
        now: Instant,
        now_ms: u64,
    ) -> BindOutcome {
        let Some(bp) = proto::parse_bind(pkt) else {
            self.metrics.inc_drop(DropReason::BadPacket);
            return BindOutcome::Silent;
        };
        if bp.role > ROLE_RESP {
            self.metrics.inc_drop(DropReason::BadPacket);
            return BindOutcome::Silent;
        }
        if !auth::ts_in_window(bp.ts_ms, now_ms, self.limits.bind_window_s) {
            self.metrics.inc_drop(DropReason::Expired);
            return BindOutcome::Silent;
        }
        if !auth::verify_auth(key, &bp.tag, bp.role, &bp.nonce, bp.ts_ms, &bp.auth) {
            self.metrics.inc_drop(DropReason::Auth);
            return BindOutcome::Silent;
        }
        // 鉴权通过后才消费 nonce，避免未鉴权的包污染表。
        if !self.nonces.insert(bp.tag, bp.role, bp.nonce, now) {
            self.metrics.inc_drop(DropReason::Replay);
            return BindOutcome::Silent;
        }

        let kind = match (src, tcp_tx) {
            (Some(addr), None) => EndpointKind::Udp(addr),
            (None, Some(tx)) => EndpointKind::Tcp(tx),
            (Some(addr), Some(tx)) => {
                // 不应同时出现；以 TCP 为准（TCP 降级通道携带显式连接身份）。
                let _ = addr;
                EndpointKind::Tcp(tx)
            }
            (None, None) => return BindOutcome::Silent,
        };
        self.bind_endpoint(bp.tag, bp.role, kind, now, now_ms)
    }

    fn session_expired(&self, tag: &Tag, now: Instant) -> bool {
        match self.sessions.get(tag) {
            None => false,
            Some(s) => {
                now.saturating_duration_since(s.last_seen) > self.limits.idle_timeout
                    || now.saturating_duration_since(s.created) > self.limits.tag_ttl
            }
        }
    }

    fn drop_session(&mut self, tag: &Tag) {
        if let Some(sess) = self.sessions.remove(tag) {
            for ep in sess.ends.iter().flatten() {
                if let EndpointKind::Udp(addr) = &ep.kind {
                    self.by_addr.remove(addr);
                }
            }
        }
    }

    fn bind_endpoint(
        &mut self,
        tag: Tag,
        role: u8,
        kind: EndpointKind,
        now: Instant,
        now_ms: u64,
    ) -> BindOutcome {
        let slot = role as usize;
        let pref = proto::tag_prefix(&tag);

        // 过期 tag 视为新会话（客户端可能在长时间静默后重新 bind）。
        if self.session_expired(&tag, now) {
            debug!(tag = %pref, "会话已过期，按新 bind 处理");
            self.drop_session(&tag);
        }

        // 阶段 1：只读判断是否 tag_full。
        let mut reject_tag_full = false;
        let mut rebind = false;
        let mut stale_udp: Option<SocketAddr> = None;
        if let Some(sess) = self.sessions.get(&tag) {
            if let Some(old) = &sess.ends[slot] {
                let same = match (&old.kind, &kind) {
                    (EndpointKind::Udp(a), EndpointKind::Udp(b)) => a == b,
                    // TCP 每次 bind 都是一个新连接，旧连接由客户端关闭；直接覆盖。
                    _ => false,
                };
                if !same {
                    let hot =
                        now.saturating_duration_since(old.last_seen) < self.limits.rebind_guard;
                    match &kind {
                        // UDP 热槽位来自不同地址 → 视为第三端点（tag_full）。
                        EndpointKind::Udp(_) if hot => reject_tag_full = true,
                        _ => {
                            rebind = true;
                            if let EndpointKind::Udp(a) = &old.kind {
                                stale_udp = Some(*a);
                            }
                        }
                    }
                }
            }
        }
        if reject_tag_full {
            self.metrics.inc_drop(DropReason::TagFull);
            return BindOutcome::Reject {
                reply: proto::bind_err_packet(&tag, proto::ERR_TAG_FULL),
                code: proto::ERR_TAG_FULL,
            };
        }

        // 阶段 2：写入。
        if !self.sessions.contains_key(&tag) {
            if self.sessions.len() >= self.limits.max_sessions {
                self.metrics.inc_drop(DropReason::MaxSessions);
                return BindOutcome::Reject {
                    reply: proto::bind_err_packet(&tag, proto::ERR_BUSY),
                    code: proto::ERR_BUSY,
                };
            }
            let bucket = self
                .limits
                .per_tag_bytes_per_s
                .map(|rate| TokenBucket::new(rate, (rate * 0.1).max(64.0 * 1024.0), now));
            self.sessions.insert(
                tag,
                TagSession {
                    tag,
                    ends: [None, None],
                    created: now,
                    last_seen: now,
                    bucket,
                },
            );
        }

        if let Some(addr) = stale_udp {
            self.by_addr.remove(&addr);
        }

        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);

        // 先把地址索引写进去（kind 需要按值保存到端点里）。
        if let EndpointKind::Udp(addr) = kind {
            if let Some(sess) = self.sessions.get_mut(&tag) {
                sess.ends[slot] = Some(Endpoint {
                    kind: EndpointKind::Udp(addr),
                    id,
                    last_seen: now,
                });
                sess.last_seen = now;
            }
            self.by_addr.insert(addr, (tag, role));
        } else {
            let EndpointKind::Tcp(tx) = kind else {
                unreachable!()
            };
            if let Some(sess) = self.sessions.get_mut(&tag) {
                sess.ends[slot] = Some(Endpoint {
                    kind: EndpointKind::Tcp(tx),
                    id,
                    last_seen: now,
                });
                sess.last_seen = now;
            }
        }

        self.metrics.set_sessions(self.sessions.len());
        let expires_in_s = self
            .sessions
            .get(&tag)
            .map(|s| {
                self.limits
                    .tag_ttl
                    .saturating_sub(now.saturating_duration_since(s.created))
                    .as_secs()
                    .clamp(1, u32::MAX as u64) as u32
            })
            .unwrap_or(1);

        if rebind {
            self.metrics.add_rebind();
            info!(tag = %pref, role, "端点重绑定（NAT rebinding），已覆盖旧地址");
        } else {
            debug!(tag = %pref, role, "bind 成功");
        }

        BindOutcome::Ok {
            tag,
            role,
            id,
            reply: proto::bind_ok_packet(&tag, expires_in_s, now_ms),
            rebind,
            expires_in_s,
        }
    }

    /// UDP 数据面：按源地址查表并成对转发。
    pub fn forward_udp(&mut self, src: SocketAddr, payload: &[u8], now: Instant) -> Option<Egress> {
        let Some((tag, role)) = self.by_addr.get(&src).copied() else {
            self.metrics.inc_drop(DropReason::UnknownSource);
            return None;
        };
        self.forward(tag, role, payload, now)
    }

    /// 按 (tag, role) 转发；TCP 通道复用同一路径。
    pub fn forward(&mut self, tag: Tag, role: u8, payload: &[u8], now: Instant) -> Option<Egress> {
        if !self.sessions.contains_key(&tag) {
            self.metrics.inc_drop(DropReason::UnknownSource);
            return None;
        }
        // 全局带宽桶先扣（避免与 sessions 的可变借用冲突）。
        if let Some(g) = &mut self.global {
            if !g.try_consume(payload.len(), now) {
                self.metrics.inc_drop(DropReason::GlobalRateLimit);
                return None;
            }
        }

        let sess = self.sessions.get_mut(&tag)?;
        let slot = (role as usize).min(1);
        if let Some(b) = &mut sess.bucket {
            if !b.try_consume(payload.len(), now) {
                self.metrics.inc_drop(DropReason::RateLimit);
                return None;
            }
        }
        sess.last_seen = now;
        if let Some(ep) = &mut sess.ends[slot] {
            ep.last_seen = now;
        }

        let peer_idx = 1 - slot;
        let Some(peer) = sess.ends[peer_idx].as_ref() else {
            self.metrics.inc_drop(DropReason::Unpaired);
            return None;
        };
        trace!(tag = %proto::tag_prefix(&tag), bytes = payload.len(), "转发");
        Some(match &peer.kind {
            EndpointKind::Udp(addr) => Egress::udp(*addr, payload.to_vec()),
            EndpointKind::Tcp(tx) => Egress::tcp(tx.clone(), proto::tcp_frame(CHAN_DATA, payload)),
        })
    }

    /// TCP 心跳帧：只刷新 last_seen。
    pub fn touch(&mut self, tag: &Tag, role: u8, now: Instant) {
        if let Some(sess) = self.sessions.get_mut(tag) {
            sess.last_seen = now;
            let slot = (role as usize).min(1);
            if let Some(ep) = &mut sess.ends[slot] {
                ep.last_seen = now;
            }
        }
    }

    /// 显式解绑（客户端 Bye / TCP 断开）。返回需要发给对端的 Bye。
    pub fn unbind(&mut self, tag: &Tag, role: u8, id: Option<u64>) -> Vec<Egress> {
        let slot = (role as usize).min(1);
        let Some(sess) = self.sessions.get(tag) else {
            return Vec::new();
        };
        if let Some(expected) = id {
            match &sess.ends[slot] {
                Some(ep) if ep.id == expected => {}
                // 旧连接退出：端点已被新连接覆盖，不要误删。
                _ => return Vec::new(),
            }
        }
        let peer_kind = sess.ends[1 - slot].as_ref().map(|e| e.kind.clone());
        let removed_kind = sess.ends[slot].as_ref().map(|e| e.kind.clone());
        let peer_remains = peer_kind.is_some();

        let mut out = Vec::new();
        if let Some(kind) = peer_kind {
            out.push(bye_egress(tag, &kind));
        }
        if !peer_remains {
            self.drop_session(tag);
        } else if let Some(sess) = self.sessions.get_mut(tag) {
            sess.ends[slot] = None;
        }
        if let Some(EndpointKind::Udp(addr)) = removed_kind {
            self.by_addr.remove(&addr);
        }
        self.metrics.set_sessions(self.sessions.len());
        debug!(tag = %proto::tag_prefix(tag), role, "端点解绑");
        out
    }

    /// 按 UDP 源地址解绑（客户端 Bye），要求 tag 匹配以避免误伤。
    pub fn unbind_addr(&mut self, src: SocketAddr, tag: &Tag) -> Vec<Egress> {
        match self.by_addr.get(&src).copied() {
            Some((t, role)) if t == *tag => self.unbind(tag, role, None),
            _ => Vec::new(),
        }
    }

    /// 定时回收：空闲 60s 或 tag 生命周期 48h 到期。返回发给被回收端点的 Bye。
    pub fn gc(&mut self, now: Instant) -> Vec<Egress> {
        let idle = self.limits.idle_timeout;
        let ttl = self.limits.tag_ttl;
        let dead: Vec<Tag> = self
            .sessions
            .iter()
            .filter(|(_, s)| {
                now.saturating_duration_since(s.last_seen) > idle
                    || now.saturating_duration_since(s.created) > ttl
            })
            .map(|(t, _)| *t)
            .collect();
        let mut out = Vec::new();
        for tag in dead {
            let Some(sess) = self.sessions.remove(&tag) else {
                continue;
            };
            for ep in sess.ends.iter().flatten() {
                out.push(bye_egress(&tag, &ep.kind));
                if let EndpointKind::Udp(addr) = &ep.kind {
                    self.by_addr.remove(addr);
                }
            }
            debug!(tag = %proto::tag_prefix(&tag), "会话回收");
        }
        if !out.is_empty() {
            self.metrics.set_sessions(self.sessions.len());
        }
        out
    }

    /// 关停时清空所有会话并生成 Bye。
    pub fn drain_all(&mut self) -> Vec<Egress> {
        let tags: Vec<Tag> = self.sessions.keys().copied().collect();
        let mut out = Vec::new();
        for tag in tags {
            if let Some(sess) = self.sessions.remove(&tag) {
                for ep in sess.ends.iter().flatten() {
                    out.push(bye_egress(&tag, &ep.kind));
                    if let EndpointKind::Udp(addr) = &ep.kind {
                        self.by_addr.remove(addr);
                    }
                }
            }
        }
        self.metrics.set_sessions(0);
        out
    }
}

fn bye_egress(tag: &Tag, kind: &EndpointKind) -> Egress {
    match kind {
        EndpointKind::Udp(addr) => Egress::udp(*addr, proto::bye_packet(tag)),
        EndpointKind::Tcp(tx) => Egress::tcp(
            tx.clone(),
            proto::tcp_frame(CHAN_CTRL, &proto::bye_packet(tag)),
        ),
    }
}

/// 测试便利：`BindOutcome::Ok` 判定（避免在断言里写长 match）。
#[cfg(test)]
trait OutcomeExt {
    fn ok_(self) -> bool;
}
#[cfg(test)]
impl OutcomeExt for BindOutcome {
    fn ok_(self) -> bool {
        matches!(self, BindOutcome::Ok { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::compute_auth;
    use crate::proto::{Nonce, BIND_LEN};
    use std::time::Duration;

    const KEY: &[u8] = b"relay-test-key-0123456789abcdef";

    fn test_limits() -> Limits {
        Limits {
            max_sessions: 8,
            per_tag_bytes_per_s: None,
            global_bytes_per_s: None,
            tag_ttl: Duration::from_secs(48 * 3600),
            idle_timeout: Duration::from_secs(60),
            bind_window_s: 300,
            nonce_capacity: 64,
            rebind_guard: Duration::from_millis(0),
        }
    }

    fn state_with(limits: Limits) -> State {
        let metrics = Arc::new(Metrics::new());
        State::new(limits, metrics)
    }

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    fn bind_pkt(tag: Tag, role: u8, nonce: Nonce, ts: u64) -> Vec<u8> {
        let auth = compute_auth(KEY, &tag, role, &nonce, ts);
        proto::bind_packet(&tag, role, &nonce, ts, &auth)
    }

    fn ok_bind(st: &mut State, tag: Tag, role: u8, port: u16, nonce: u8) -> BindOutcome {
        let now = Instant::now();
        st.bind(
            KEY,
            &bind_pkt(tag, role, [nonce; 16], 1_000_000),
            Some(addr(port)),
            None,
            now,
            1_000_000,
        )
    }

    #[test]
    fn pairing_and_forwarding() {
        let mut st = state_with(test_limits());
        let tag = [5u8; 16];
        let a = addr(40001);
        let b = addr(40002);
        assert!(matches!(
            ok_bind(&mut st, tag, ROLE_INIT, 40001, 1),
            BindOutcome::Ok { .. }
        ));
        assert!(matches!(
            ok_bind(&mut st, tag, ROLE_RESP, 40002, 2),
            BindOutcome::Ok { .. }
        ));
        assert_eq!(st.session_count(), 1);

        let eg = st.forward_udp(a, b"hello", Instant::now()).expect("A→B");
        match &eg.kind {
            EgressKind::Udp { to, data } => {
                assert_eq!(*to, b);
                assert_eq!(data, b"hello");
            }
            _ => panic!("应为 UDP 转发"),
        }
        let eg = st.forward_udp(b, b"world", Instant::now()).expect("B→A");
        match &eg.kind {
            EgressKind::Udp { to, .. } => assert_eq!(*to, a),
            _ => panic!("应为 UDP 转发"),
        }
        // 未 bind 的来源静默丢弃。
        assert!(st.forward_udp(addr(40003), b"x", Instant::now()).is_none());
        assert_eq!(
            st.metrics.dropped(DropReason::UnknownSource),
            1,
            "未知来源必须计数"
        );
        assert_eq!(st.metrics.dropped(DropReason::RateLimit), 0);
    }

    #[test]
    fn third_endpoint_rejected_when_hot() {
        let mut limits = test_limits();
        limits.rebind_guard = Duration::from_secs(2);
        let mut st = state_with(limits);
        let tag = [6u8; 16];
        assert!(matches!(
            ok_bind(&mut st, tag, ROLE_INIT, 41001, 1),
            BindOutcome::Ok { .. }
        ));
        // 同 role、不同地址、槽位热的 → tag_full。
        let now = Instant::now();
        let out = st.bind(
            KEY,
            &bind_pkt(tag, ROLE_INIT, [9; 16], 1_000_000),
            Some(addr(41002)),
            None,
            now,
            1_000_000,
        );
        match out {
            BindOutcome::Reject { code, .. } => assert_eq!(code, proto::ERR_TAG_FULL),
            _ => panic!("第三端点必须被 tag_full 拒绝"),
        }
        assert_eq!(st.metrics.dropped(DropReason::TagFull), 1);
        // 原端点仍可用。
        assert!(
            st.forward_udp(addr(41001), b"x", Instant::now()).is_none(),
            "配对未完成应 unpaired"
        );
    }

    #[test]
    fn nat_rebind_replaces_stale_endpoint() {
        let mut limits = test_limits();
        limits.rebind_guard = Duration::from_millis(50);
        let mut st = state_with(limits);
        let tag = [7u8; 16];
        assert!(matches!(
            ok_bind(&mut st, tag, ROLE_INIT, 42001, 1),
            BindOutcome::Ok { .. }
        ));
        assert!(matches!(
            ok_bind(&mut st, tag, ROLE_RESP, 42002, 2),
            BindOutcome::Ok { .. }
        ));
        // 让旧端点变冷（模拟 NAT 映射失效后客户端重 bind）。
        let later = Instant::now() + Duration::from_millis(200);
        let out = st.bind(
            KEY,
            &bind_pkt(tag, ROLE_INIT, [3; 16], 1_000_000),
            Some(addr(42099)),
            None,
            later,
            1_000_000,
        );
        assert!(matches!(out, BindOutcome::Ok { rebind: true, .. }));
        assert_eq!(st.metrics.rebinds(), 1, "重绑定必须计数");
        // 新地址可用，旧地址被移除。
        assert!(st.forward_udp(addr(42001), b"old", later).is_none());
        let eg = st
            .forward_udp(addr(42099), b"new", later)
            .expect("新地址应可转发");
        match eg.kind {
            EgressKind::Udp { to, .. } => assert_eq!(to, addr(42002)),
            _ => panic!(),
        }
    }

    #[test]
    fn idle_gc_removes_and_sends_bye() {
        let mut limits = test_limits();
        limits.idle_timeout = Duration::from_millis(100);
        let mut st = state_with(limits);
        let tag = [8u8; 16];
        assert!(matches!(
            ok_bind(&mut st, tag, ROLE_INIT, 43001, 1),
            BindOutcome::Ok { .. }
        ));
        assert!(matches!(
            ok_bind(&mut st, tag, ROLE_RESP, 43002, 2),
            BindOutcome::Ok { .. }
        ));
        let out = st.gc(Instant::now() + Duration::from_millis(500));
        assert_eq!(st.session_count(), 0);
        assert_eq!(out.len(), 2, "两个端点都应收到 Bye");
        assert!(st.forward_udp(addr(43001), b"x", Instant::now()).is_none());
    }

    #[test]
    fn tag_ttl_expiry() {
        let mut limits = test_limits();
        limits.tag_ttl = Duration::from_millis(100);
        let mut st = state_with(limits);
        let tag = [9u8; 16];
        assert!(matches!(
            ok_bind(&mut st, tag, ROLE_INIT, 44001, 1),
            BindOutcome::Ok { .. }
        ));
        let out = st.gc(Instant::now() + Duration::from_millis(500));
        assert_eq!(st.session_count(), 0);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn per_tag_rate_limit_drops() {
        let mut limits = test_limits();
        limits.per_tag_bytes_per_s = Some(1_000.0);
        let mut st = state_with(limits);
        let tag = [10u8; 16];
        assert!(matches!(
            ok_bind(&mut st, tag, ROLE_INIT, 45001, 1),
            BindOutcome::Ok { .. }
        ));
        assert!(matches!(
            ok_bind(&mut st, tag, ROLE_RESP, 45002, 2),
            BindOutcome::Ok { .. }
        ));
        let now = Instant::now();
        // 60KB 突发容量；70KB 一包直接超限。
        assert!(st
            .forward_udp(addr(45001), &vec![0u8; 70_000], now)
            .is_none());
        assert_eq!(st.metrics.dropped(DropReason::RateLimit), 1);
        assert!(st.forward_udp(addr(45001), b"small", now).is_some());
    }

    #[test]
    fn max_sessions_enforced() {
        let mut limits = test_limits();
        limits.max_sessions = 1;
        let mut st = state_with(limits);
        assert!(matches!(
            ok_bind(&mut st, [1u8; 16], ROLE_INIT, 46001, 1),
            BindOutcome::Ok { .. }
        ));
        match ok_bind(&mut st, [2u8; 16], ROLE_INIT, 46002, 2) {
            BindOutcome::Reject { code, .. } => assert_eq!(code, proto::ERR_BUSY),
            _ => panic!("超过 max_sessions 必须拒绝"),
        }
    }

    #[test]
    fn replay_and_bad_auth_silent() {
        let mut st = state_with(test_limits());
        let tag = [11u8; 16];
        let pkt = bind_pkt(tag, ROLE_INIT, [1; 16], 1_000_000);
        assert_eq!(pkt.len(), BIND_LEN);
        let now = Instant::now();
        assert!(matches!(
            st.bind(KEY, &pkt, Some(addr(47001)), None, now, 1_000_000),
            BindOutcome::Ok { .. }
        ));
        // 同一 nonce 重放 → 静默。
        assert!(matches!(
            st.bind(KEY, &pkt, Some(addr(47002)), None, now, 1_000_000),
            BindOutcome::Silent
        ));
        assert_eq!(st.metrics.dropped(DropReason::Replay), 1);
        // 错误 key → 静默。
        assert!(matches!(
            st.bind(
                b"other-key",
                &bind_pkt(tag, ROLE_RESP, [2; 16], 1_000_000),
                Some(addr(47003)),
                None,
                now,
                1_000_000
            ),
            BindOutcome::Silent
        ));
        assert_eq!(st.metrics.dropped(DropReason::Auth), 1);
        // 过期时间戳 → 静默。
        assert!(matches!(
            st.bind(
                KEY,
                &bind_pkt(tag, ROLE_RESP, [3; 16], 1_000_000),
                Some(addr(47004)),
                None,
                now,
                9_000_000
            ),
            BindOutcome::Silent
        ));
        assert_eq!(st.metrics.dropped(DropReason::Expired), 1);
    }

    #[test]
    fn unbind_removes_endpoint_and_notifies_peer() {
        let mut st = state_with(test_limits());
        let tag = [12u8; 16];
        assert!(matches!(
            ok_bind(&mut st, tag, ROLE_INIT, 48001, 1),
            BindOutcome::Ok { .. }
        ));
        assert!(ok_bind(&mut st, tag, ROLE_RESP, 48002, 2).ok_());
        let out = st.unbind(&tag, ROLE_INIT, None);
        assert_eq!(out.len(), 1, "对端应收到 Bye");
        assert_eq!(st.session_count(), 1, "对端仍绑定");
        assert!(st.forward_udp(addr(48001), b"x", Instant::now()).is_none());
        let out = st.unbind(&tag, ROLE_RESP, None);
        assert!(out.is_empty());
        assert_eq!(st.session_count(), 0);
    }
}
