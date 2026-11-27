// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
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

use tokio::sync::{mpsc, watch};
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

/// 数据面的发送者身份：必须同时匹配当前代际与承载方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndpointSource {
    Udp { addr: SocketAddr, id: u64 },
    Tcp { id: u64 },
}

pub struct Endpoint {
    pub kind: EndpointKind,
    /// 唯一 id：旧连接不能转发、保活或移除新端点。
    pub id: u64,
    pub last_seen: Instant,
    /// 槽位替换/移除时释放 sender，唤醒并终止旧 TCP reader/writer。
    _closed: watch::Sender<bool>,
}

impl Endpoint {
    fn matches(&self, source: EndpointSource) -> bool {
        match (&self.kind, source) {
            (EndpointKind::Udp(expected), EndpointSource::Udp { addr, id }) => {
                *expected == addr && self.id == id
            }
            (EndpointKind::Tcp(_), EndpointSource::Tcp { id }) => self.id == id,
            _ => false,
        }
    }
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
        /// 端点 id（所有数据、心跳和解绑都必须验证此代际）。
        id: u64,
        /// 当前槽位被替换/移除时 channel 关闭，旧 TCP 任务必须退出。
        closed: watch::Receiver<bool>,
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
    /// 仅 UDP 端点：源地址 → (tag, role, endpoint id)。
    by_addr: HashMap<SocketAddr, (Tag, u8, u64)>,
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
                    self.remove_addr_index(*addr, ep.id);
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
        let mut stale_udp: Option<(SocketAddr, u64)> = None;
        if let Some(sess) = self.sessions.get(&tag) {
            if let Some(old) = &sess.ends[slot] {
                let same = match (&old.kind, &kind) {
                    (EndpointKind::Udp(a), EndpointKind::Udp(b)) => a == b,
                    // TCP 的新连接不能继承旧连接的槽位身份。
                    _ => false,
                };
                if !same {
                    let hot =
                        now.saturating_duration_since(old.last_seen) < self.limits.rebind_guard;
                    if hot {
                        reject_tag_full = true;
                    } else {
                        rebind = true;
                        if let EndpointKind::Udp(a) = &old.kind {
                            stale_udp = Some((*a, old.id));
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

        if let Some((addr, id)) = stale_udp {
            self.remove_addr_index(addr, id);
        }

        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let (closed_tx, closed) = watch::channel(false);

        if let EndpointKind::Udp(addr) = &kind {
            self.by_addr.insert(*addr, (tag, role, id));
        }
        if let Some(sess) = self.sessions.get_mut(&tag) {
            // 丢弃旧 endpoint 的 lifetime sender，立即通知旧 TCP 任务退出。
            sess.ends[slot] = Some(Endpoint {
                kind,
                id,
                last_seen: now,
                _closed: closed_tx,
            });
            sess.last_seen = now;
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
            closed,
            reply: proto::bind_ok_packet(&tag, expires_in_s, now_ms),
            rebind,
            expires_in_s,
        }
    }

    /// 删除索引时验证代际，避免旧会话清理误删同地址的新绑定。
    fn remove_addr_index(&mut self, addr: SocketAddr, id: u64) {
        if matches!(self.by_addr.get(&addr), Some((_, _, current)) if *current == id) {
            self.by_addr.remove(&addr);
        }
    }

    fn is_current_source(&self, tag: &Tag, role: u8, source: EndpointSource) -> bool {
        let Some(ep) = self
            .sessions
            .get(tag)
            .and_then(|s| s.ends.get(role as usize))
            .and_then(Option::as_ref)
        else {
            return false;
        };
        if !ep.matches(source) {
            return false;
        }
        match source {
            EndpointSource::Udp { addr, id } => self.by_addr.get(&addr) == Some(&(*tag, role, id)),
            EndpointSource::Tcp { .. } => true,
        }
    }

    /// UDP 数据面：源地址索引和当前槽位的地址、代际必须全部匹配。
    pub fn forward_udp(&mut self, src: SocketAddr, payload: &[u8], now: Instant) -> Option<Egress> {
        let Some((tag, role, id)) = self.by_addr.get(&src).copied() else {
            self.metrics.inc_drop(DropReason::UnknownSource);
            return None;
        };
        self.forward(
            tag,
            role,
            EndpointSource::Udp { addr: src, id },
            payload,
            now,
        )
    }

    /// 发送者必须仍持有当前 endpoint 身份，验证通过后才扣限速桶、刷新活性。
    pub fn forward(
        &mut self,
        tag: Tag,
        role: u8,
        source: EndpointSource,
        payload: &[u8],
        now: Instant,
    ) -> Option<Egress> {
        if !self.is_current_source(&tag, role, source) {
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
        let slot = role as usize;
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

    /// 心跳只允许当前来源刷新活性；旧代际、错误承载方式/地址一律拒绝。
    pub fn touch(&mut self, tag: &Tag, role: u8, source: EndpointSource, now: Instant) -> bool {
        if !self.is_current_source(tag, role, source) {
            self.metrics.inc_drop(DropReason::UnknownSource);
            return false;
        }
        if let Some(sess) = self.sessions.get_mut(tag) {
            sess.last_seen = now;
            if let Some(ep) = &mut sess.ends[role as usize] {
                ep.last_seen = now;
            }
        }
        true
    }

    /// 显式解绑（客户端 Bye / TCP 断开），必须提供当前来源身份。
    pub fn unbind(&mut self, tag: &Tag, role: u8, source: EndpointSource) -> Vec<Egress> {
        if !self.is_current_source(tag, role, source) {
            return Vec::new();
        }
        let slot = role as usize;
        let Some(sess) = self.sessions.get(tag) else {
            return Vec::new();
        };
        let peer_kind = sess.ends[1 - slot].as_ref().map(|e| e.kind.clone());
        let removed = sess.ends[slot].as_ref().map(|e| (e.kind.clone(), e.id));
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
        if let Some((EndpointKind::Udp(addr), id)) = removed {
            self.remove_addr_index(addr, id);
        }
        self.metrics.set_sessions(self.sessions.len());
        debug!(tag = %proto::tag_prefix(tag), role, "端点解绑");
        out
    }

    /// UDP Bye 同样验证地址索引、tag、承载方式与代际。
    pub fn unbind_addr(&mut self, src: SocketAddr, tag: &Tag) -> Vec<Egress> {
        match self.by_addr.get(&src).copied() {
            Some((t, role, id)) if t == *tag => {
                self.unbind(tag, role, EndpointSource::Udp { addr: src, id })
            }
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
                    self.remove_addr_index(*addr, ep.id);
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
                        self.remove_addr_index(*addr, ep.id);
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
    fn full_nonce_cache_rejects_replay_and_fresh_bind_across_transports() {
        for first_tcp in [false, true] {
            let mut limits = test_limits();
            limits.nonce_capacity = 2;
            let mut st = state_with(limits);
            let tag = [13; 16];
            let now = Instant::now();
            let pkt = bind_pkt(tag, ROLE_INIT, [1; 16], 1_000_000);
            let (tx, _rx) = mpsc::channel(8);
            let first = if first_tcp {
                st.bind(KEY, &pkt, None, Some(tx), now, 1_000_000)
            } else {
                st.bind(KEY, &pkt, Some(addr(49001)), None, now, 1_000_000)
            };
            assert!(first.ok_());
            assert!(ok_bind(&mut st, tag, ROLE_RESP, 49002, 2).ok_());
            let current = st.sessions[&tag].ends[0].as_ref().unwrap().id;
            for candidate in [pkt, bind_pkt(tag, ROLE_INIT, [3; 16], 1_000_000)] {
                let (tx, _rx) = mpsc::channel(8);
                let replay = if first_tcp {
                    st.bind(KEY, &candidate, Some(addr(49003)), None, now, 1_000_000)
                } else {
                    st.bind(KEY, &candidate, None, Some(tx), now, 1_000_000)
                };
                assert!(matches!(replay, BindOutcome::Silent));
                assert_eq!(st.sessions[&tag].ends[0].as_ref().unwrap().id, current);
                assert_eq!(st.nonces.len(), 2);
            }
        }
    }

    #[test]
    fn hot_slots_reject_replacement_in_every_transport_direction() {
        for old_tcp in [false, true] {
            for new_tcp in [false, true] {
                let mut limits = test_limits();
                limits.rebind_guard = Duration::from_secs(2);
                let mut st = state_with(limits);
                let tag = [14; 16];
                let now = Instant::now();
                let (tx, _rx) = mpsc::channel(8);
                let old = st.bind(
                    KEY,
                    &bind_pkt(tag, ROLE_INIT, [1; 16], 1_000_000),
                    (!old_tcp).then_some(addr(49101)),
                    old_tcp.then_some(tx),
                    now,
                    1_000_000,
                );
                let BindOutcome::Ok { id, closed, .. } = old else {
                    panic!("first bind failed");
                };
                let (tx, _rx) = mpsc::channel(8);
                let replacement = st.bind(
                    KEY,
                    &bind_pkt(tag, ROLE_INIT, [2; 16], 1_000_000),
                    (!new_tcp).then_some(addr(49102)),
                    new_tcp.then_some(tx),
                    now + Duration::from_millis(1),
                    1_000_000,
                );
                assert!(
                    matches!(
                        replacement,
                        BindOutcome::Reject {
                            code: proto::ERR_TAG_FULL,
                            ..
                        }
                    ),
                    "old_tcp={old_tcp}, new_tcp={new_tcp}"
                );
                assert_eq!(st.sessions[&tag].ends[0].as_ref().unwrap().id, id);
                assert!(matches!(closed.has_changed(), Ok(false)));
            }
        }
    }

    #[test]
    fn replaced_tcp_cannot_forward_touch_or_unbind_current_generation() {
        let mut limits = test_limits();
        limits.rebind_guard = Duration::from_millis(50);
        let mut st = state_with(limits);
        let tag = [15; 16];
        let t0 = Instant::now();
        let (tx, _rx) = mpsc::channel(8);
        let BindOutcome::Ok {
            id: old_id, closed, ..
        } = st.bind(
            KEY,
            &bind_pkt(tag, ROLE_INIT, [1; 16], 1_000_000),
            None,
            Some(tx),
            t0,
            1_000_000,
        )
        else {
            panic!("old TCP bind failed");
        };
        assert!(ok_bind(&mut st, tag, ROLE_RESP, 49202, 2).ok_());
        let t1 = t0 + Duration::from_millis(100);
        let (tx, _rx) = mpsc::channel(8);
        let BindOutcome::Ok { id: new_id, .. } = st.bind(
            KEY,
            &bind_pkt(tag, ROLE_INIT, [3; 16], 1_000_000),
            None,
            Some(tx),
            t1,
            1_000_000,
        ) else {
            panic!("replacement TCP bind failed");
        };
        assert_ne!(old_id, new_id);
        assert!(closed.has_changed().is_err(), "旧连接必须收到槽位失效通知");
        let old = EndpointSource::Tcp { id: old_id };
        let t2 = t1 + Duration::from_millis(10);
        assert!(st.forward(tag, ROLE_INIT, old, b"old", t2).is_none());
        assert!(!st.touch(&tag, ROLE_INIT, old, t2));
        assert!(st.unbind(&tag, ROLE_INIT, old).is_empty());
        assert_eq!(st.sessions[&tag].last_seen, t1);
        assert_eq!(st.sessions[&tag].ends[0].as_ref().unwrap().last_seen, t1);
        let current = EndpointSource::Tcp { id: new_id };
        assert!(st.forward(tag, 255, current, b"bad role", t2).is_none());
        assert!(!st.touch(&tag, 255, current, t2));
        assert!(st.unbind(&tag, 255, current).is_empty());
        let eg = st.forward(tag, ROLE_INIT, current, b"new", t2).unwrap();
        assert!(matches!(eg.kind, EgressKind::Udp { to, .. } if to == addr(49202)));
        assert!(st.touch(&tag, ROLE_INIT, current, t2));
    }

    #[test]
    fn udp_sources_require_current_address_transport_and_generation() {
        let mut st = state_with(test_limits());
        let tag = [16; 16];
        let BindOutcome::Ok { id: old_id, .. } = ok_bind(&mut st, tag, ROLE_INIT, 49301, 1) else {
            panic!("old UDP bind failed");
        };
        assert!(ok_bind(&mut st, tag, ROLE_RESP, 49302, 2).ok_());
        let BindOutcome::Ok { id, .. } = ok_bind(&mut st, tag, ROLE_INIT, 49303, 3) else {
            panic!("replacement UDP bind failed");
        };
        let last_seen = st.sessions[&tag].last_seen;
        let later = last_seen + Duration::from_millis(10);
        assert!(st.forward_udp(addr(49301), b"old", later).is_none());
        // 模拟过期索引残留：仅凭可查到 tag/role 不能授权旧地址或错误代际。
        st.by_addr.insert(addr(49301), (tag, ROLE_INIT, id));
        for source in [
            EndpointSource::Udp {
                addr: addr(49301),
                id,
            },
            EndpointSource::Udp {
                addr: addr(49303),
                id: old_id,
            },
            EndpointSource::Tcp { id },
        ] {
            assert!(st.forward(tag, ROLE_INIT, source, b"bad", later).is_none());
            assert!(!st.touch(&tag, ROLE_INIT, source, later));
            assert!(st.unbind(&tag, ROLE_INIT, source).is_empty());
        }
        assert!(st.forward_udp(addr(49301), b"stale index", later).is_none());
        assert!(st.unbind_addr(addr(49301), &tag).is_empty());
        assert_eq!(st.sessions[&tag].last_seen, last_seen);
        assert!(st.forward_udp(addr(49303), b"current", later).is_some());
    }

    #[test]
    fn old_udp_session_cleanup_preserves_new_address_mapping() {
        let mut st = state_with(test_limits());
        let old_tag = [17; 16];
        let new_tag = [18; 16];
        assert!(ok_bind(&mut st, old_tag, ROLE_INIT, 49401, 1).ok_());
        assert!(ok_bind(&mut st, new_tag, ROLE_INIT, 49401, 2).ok_());
        assert!(ok_bind(&mut st, new_tag, ROLE_RESP, 49402, 3).ok_());
        st.drop_session(&old_tag);
        let eg = st
            .forward_udp(addr(49401), b"new tag", Instant::now())
            .unwrap();
        assert!(matches!(eg.kind, EgressKind::Udp { to, .. } if to == addr(49402)));
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
        let out = st.unbind_addr(addr(48001), &tag);
        assert_eq!(out.len(), 1, "对端应收到 Bye");
        assert_eq!(st.session_count(), 1, "对端仍绑定");
        assert!(st.forward_udp(addr(48001), b"x", Instant::now()).is_none());
        let out = st.unbind_addr(addr(48002), &tag);
        assert!(out.is_empty());
        assert_eq!(st.session_count(), 0);
    }
}
