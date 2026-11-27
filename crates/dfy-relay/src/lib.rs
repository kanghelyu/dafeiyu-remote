// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Dafeiyu Remote contributors
// Copyright (c) 2026 Dafeiyu Remote contributors
//! # dfy-relay — DFYP/1 中继服务器（大肥鱼远程）
//!
//! 只做 `tag → (端点A, 端点B)` 的成对转发：不解析 QUIC、不终结 TLS，
//! 端到端加密由客户端负责。数据面支持 UDP（主）与 TCP 降级通道（可互通桥接）。
//!
//! 模块：
//! - [`proto`]：信封协议（Bind / BindOk / BindErr / Bye / TCP 帧）；
//! - [`auth`]：HMAC-SHA256 bind 鉴权、±300s 时间窗、nonce 防重放；
//! - [`state`]：tag 会话表（配对、重绑定、GC、限速）；
//! - [`udp`] / [`tcp`]：数据面收包循环；[`admin`]：本机 HTTP 健康/指标；
//! - [`config`]：TOML 配置；[`selftest`]：进程内端到端自测；
//! - [`relay`]：把上述部件组装成可启动/可优雅关闭的服务。
//!
//! ## 客户端集成（QUIC over relay）
//!
//! 本模块同时提供**客户端侧**能力（[`RelayClientConfig`] / [`RelaySocket`] /
//! [`bind_relay_socket`]），供 Agent 与 Client 复用中继的 bind 协议：
//!
//! ```text
//! 两端各自：bind_relay_socket(cfg).await        // 本地 UDP socket + 中继 bind + 20s 保活
//!          → quinn::Endpoint::new(..., relay.try_clone_socket()?, ...)
//! Client ：endpoint.connect(relay_addr, "localhost")   // 目标永远是中继地址
//! Agent  ：endpoint.accept()                           // 收到的包来自中继地址
//! ```
//!
//! 中继不区分方向，只按 `(tag, role)` 查表转发；两端都只把「中继地址」当作对端
//! 地址，QUIC 靠 connection ID 区分连接，因此可以正常完成握手与 DATAGRAM 传输。
//! 详细调用示例、字段说明与失败模式见 `README.md` 的「客户端集成」一节。

pub mod admin;
pub mod auth;
pub mod config;
pub mod metrics;
pub mod proto;
pub mod ratelimit;
pub mod relay;
pub mod selftest;
pub mod state;
pub mod tcp;
pub mod udp;

use std::io;
use std::net::{SocketAddr, UdpSocket as StdUdpSocket};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use tokio::net::UdpSocket as TokioUdpSocket;

use crate::auth::compute_auth;
use crate::config::RelayKey;
use crate::proto::{Nonce, PacketClass, Tag};

/// 当前 Unix 毫秒时间戳（bind 时间窗与 BindOk.server_ms 使用）。
pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 等待一次 BindOk 的总超时（含重发）。
pub const DEFAULT_BIND_TIMEOUT: Duration = Duration::from_secs(5);

/// BindRequest 重发间隔；每次重发都换新 nonce（中继按 `(tag, role, nonce)` 去重）。
pub const DEFAULT_BIND_RETRY_INTERVAL: Duration = Duration::from_millis(300);

/// 保活周期：中继 `idle_timeout_s` 默认 60s，20s 一次留足余量。
pub const DEFAULT_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(20);

/// 客户端在中继会话里的角色（映射到会话表的两个槽位）。
///
/// 中继按 `(tag, role)` 存两个端点：`ROLE_INIT`（槽 0）与 `ROLE_RESP`（槽 1）。
/// **同一 tag 的两端必须使用不同 role**；两端用同一个 role 会互相顶替，
/// 表现为 `BindErr(tag_full)` 或 QUIC 握手超时。
///
/// 顺序语义：中继不关心谁先 bind，但实战建议长驻的 Agent 用 [`Role::Agent`]
/// 先 bind，拨号的 Client 用 [`Role::Client`] 后 bind（重启 Agent 时若旧端点仍在
/// `rebind_guard` 窗口内会先收到 `tag_full`，退避 2s 再试即可，见 README）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// 槽 0（`state::ROLE_INIT`）：长驻端（被控端 Agent）。
    Agent,
    /// 槽 1（`state::ROLE_RESP`）：拨号端（控制端 Client）。
    Client,
}

impl Role {
    /// 映射到协议里的 1 字节 role。
    pub const fn as_u8(self) -> u8 {
        match self {
            Role::Agent => crate::state::ROLE_INIT,
            Role::Client => crate::state::ROLE_RESP,
        }
    }

    /// 从协议字节还原 role；非法值返回 `None`（中继对 `role > 1` 静默丢弃）。
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            crate::state::ROLE_INIT => Some(Role::Agent),
            crate::state::ROLE_RESP => Some(Role::Client),
            _ => None,
        }
    }

    /// 配置/日志里的稳定名字（`agent` / `client`）。
    pub const fn as_str(self) -> &'static str {
        match self {
            Role::Agent => "agent",
            Role::Client => "client",
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Role {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "agent" | "init" | "0" => Ok(Role::Agent),
            "client" | "resp" | "1" => Ok(Role::Client),
            other => Err(anyhow!("未知的中继角色 {:?}（可用 agent / client）", other)),
        }
    }
}

/// 中继客户端配置：连接哪个中继、用哪个 tag/role 配对、共享密钥与 nonce 种子。
#[derive(Clone)]
pub struct RelayClientConfig {
    /// 中继 UDP 地址（部署时对应 `[[relay]] addr`，如 `relay.example.com:21117`）。
    pub relay_addr: SocketAddr,
    /// 16 字节配对 tag：同一 tag + 不同 role 的两端被中继配成一对。
    /// 建议用随机值（`rand::random::<[u8; 16]>()`），不要用可猜的字符串。
    pub tag: Tag,
    /// 本端角色：Agent（槽 0）或 Client（槽 1），两端必须不同。
    pub role: Role,
    /// 与中继 `relay_key` 一致的 32 字节共享密钥（HMAC-SHA256 的 key）。
    pub key: [u8; 32],
    /// nonce 种子（u64）。同一进程每次 bind/保活会在此基础上递增生成 16 字节 nonce。
    pub nonce: u64,
}

impl RelayClientConfig {
    /// 便捷构造：直接使用 [`RelayKey`]（中继配置里的共享密钥类型）。
    pub fn with_relay_key(
        relay_addr: SocketAddr,
        tag: Tag,
        role: Role,
        key: &RelayKey,
        nonce: u64,
    ) -> Self {
        Self {
            relay_addr,
            tag,
            role,
            key: *key.as_bytes(),
            nonce,
        }
    }
}

impl std::fmt::Debug for RelayClientConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayClientConfig")
            .field("relay_addr", &self.relay_addr)
            .field("tag", &proto::tag_prefix(&self.tag).to_string())
            .field("role", &self.role)
            .field("key", &"<redacted>")
            .field("nonce", &self.nonce)
            .finish()
    }
}

/// [`bind_relay_socket`] 的可调参数（默认值满足生产；测试可缩短时间片）。
#[derive(Debug, Clone)]
pub struct RelaySocketOptions {
    /// 等待 BindOk 的总超时。
    pub bind_timeout: Duration,
    /// 一次 BindRequest 未收到回应时的重发间隔（每次重发换新 nonce）。
    pub retry_interval: Duration,
    /// 保活重发周期。
    pub keepalive_interval: Duration,
}

impl Default for RelaySocketOptions {
    fn default() -> Self {
        Self {
            bind_timeout: DEFAULT_BIND_TIMEOUT,
            retry_interval: DEFAULT_BIND_RETRY_INTERVAL,
            keepalive_interval: DEFAULT_KEEPALIVE_INTERVAL,
        }
    }
}

/// 已在中继完成 bind 的客户端 UDP socket（含 20s 保活任务）。
///
/// 生命周期：`RelaySocket` 必须比使用它的 QUIC 连接活得更久——保活任务在
/// `Drop` 时被中止；停止保活后中继会在 `idle_timeout`（默认 60s）后回收会话。
pub struct RelaySocket {
    socket: StdUdpSocket,
    relay_addr: SocketAddr,
    tag: Tag,
    role: Role,
    key: [u8; 32],
    nonce_seed: u64,
    next_nonce: Arc<AtomicU64>,
    expires_in_s: u32,
    server_ms: u64,
    keepalive: Option<tokio::task::JoinHandle<()>>,
}

impl RelaySocket {
    /// 中继地址（QUIC `connect()` 的目标，也是收到的包的对端地址）。
    pub fn peer_addr(&self) -> SocketAddr {
        self.relay_addr
    }

    /// 本地 UDP 地址（`0.0.0.0:0` 绑定后实际拿到的地址）。
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// 底层 `std::net::UdpSocket`（非阻塞）。
    ///
    /// 交给 quinn 时请用 [`RelaySocket::try_clone_socket`]：`Endpoint::new` 需要
    /// owned socket，而副本与原件共享同一 UDP 端口与收包队列。quinn 接管后
    /// **不要**再在本 socket 或任何副本上 `recv`，否则会把 QUIC 包抢走。
    pub fn socket(&self) -> &StdUdpSocket {
        &self.socket
    }

    /// 复制底层 socket（dup）：与原件共享同一 UDP 端口，中继看到的源地址不变。
    ///
    /// 推荐用法：`RelaySocket` 自己保留原件，把副本交给
    /// `quinn::Endpoint::new(EndpointConfig::default(), server_config, relay.try_clone_socket()?, Arc::new(quinn::TokioRuntime))`。
    pub fn try_clone_socket(&self) -> io::Result<StdUdpSocket> {
        self.socket.try_clone()
    }

    /// 本端绑定的 tag。
    pub fn tag(&self) -> &Tag {
        &self.tag
    }

    /// 本端角色。
    pub fn role(&self) -> Role {
        self.role
    }

    /// 中继返回的 tag 剩余有效期（秒，默认 48h TTL）。
    pub fn expires_in_s(&self) -> u32 {
        self.expires_in_s
    }

    /// 中继 BindOk 里的服务器毫秒时钟（可与本地 `unix_ms()` 比较，发现时钟偏差）。
    pub fn server_clock_ms(&self) -> u64 {
        self.server_ms
    }

    /// 主动发一次 BindRequest 保活（同 tag/role，nonce 递增）。
    ///
    /// 中继对同地址重复 bind 是幂等的：回 BindOk 且不算 rebind。回包落在同一
    /// socket 的共享收包队列里——交给 quinn 后会由 quinn 当非法报文忽略；未接
    /// quinn 时调用方可自行 `recv` 验证。
    pub fn send_bind_request(&self) -> Result<()> {
        let nonce = bind_nonce(
            self.nonce_seed,
            self.next_nonce.fetch_add(1, Ordering::Relaxed),
        );
        let pkt = bind_request_packet(&self.key, &self.tag, self.role.as_u8(), &nonce);
        self.socket
            .send_to(&pkt, self.relay_addr)
            .with_context(|| format!("向中继 {} 发送保活 BindRequest 失败", self.relay_addr))?;
        Ok(())
    }

    /// 停止保活任务（一般不需要：`Drop` 会自动中止）。
    pub fn stop_keepalive(&mut self) {
        if let Some(task) = self.keepalive.take() {
            task.abort();
        }
    }
}

impl Drop for RelaySocket {
    fn drop(&mut self) {
        self.stop_keepalive();
    }
}

/// 完成中继 bind 并返回可构造 QUIC endpoint 的 socket（默认参数）。
pub async fn bind_relay_socket(cfg: &RelayClientConfig) -> Result<RelaySocket> {
    bind_relay_socket_with(cfg, &RelaySocketOptions::default()).await
}

/// 建立可交给 QUIC 使用的中继客户端 socket。
///
/// 步骤：
/// 1. 本地绑定随机端口（中继是 IPv4 则 `0.0.0.0:0`，IPv6 则 `[::]:0`），设非阻塞；
/// 2. 向 `cfg.relay_addr` 发 BindRequest，等待中继 BindOk：每次 `retry_interval`
///    没回应就换**新 nonce** 重发（复用 nonce 会被中继当重放静默丢弃），总超时
///    `bind_timeout`；收到 BindErr 立即失败并给出 tag_full/busy 的解释；
/// 3. 校验 BindOk 的 tag 回显，记录 tag TTL 与中继毫秒时钟；
/// 4. 启动保活任务：每 `keepalive_interval`（默认 20s）重发 BindRequest（nonce 递增）。
///
/// **谁先 Bind**：两端（Agent / Client）都必须在建立 QUIC 之前完成 bind，中继只
/// 把「同一 tag 的两个不同 role 槽位都 bind 过」的端点配对。先后顺序不限；未配对
/// 期间的数据包会被中继计为 `unpaired` 丢弃。建议长驻 Agent 先 bind、Client 后
/// bind：这样重启 Agent 不受拨号时序影响。
///
/// **两端如何触发 QUIC**：中继不参与 QUIC 握手——它只把 payload 原样转发。两端各自
/// 用自己的 relay socket 建 `quinn::Endpoint`，Client `connect(cfg.relay_addr)`，
/// Agent `accept()`；由于两端发出的包都先到中继、中继再从自己的 socket 发给对端，
/// 每端看到的 QUIC 对端地址都是**中继地址**，所以客户端 `connect` 的目标必须写
/// 中继地址（`server_name` 用 `"localhost"` 以匹配自签证书 SAN）；服务端收到的
/// 远端地址也会是中继地址——quinn 按 connection ID 解复用，不需要真实对端地址。
///
/// **socket 所有权**：请用 [`RelaySocket::try_clone_socket`] 的副本喂给
/// `quinn::Endpoint::new`（它要 owned `std::net::UdpSocket`；dup 共享同一 UDP
/// 端口，中继看到的源地址不变）。`RelaySocket` 自身要活到连接结束。
pub async fn bind_relay_socket_with(
    cfg: &RelayClientConfig,
    opts: &RelaySocketOptions,
) -> Result<RelaySocket> {
    if opts.retry_interval.is_zero() {
        bail!("RelaySocketOptions.retry_interval 不能为 0（会导致忙等）");
    }
    if opts.keepalive_interval.is_zero() {
        bail!("RelaySocketOptions.keepalive_interval 不能为 0");
    }

    let local: SocketAddr = if cfg.relay_addr.is_ipv4() {
        SocketAddr::from(([0u8, 0, 0, 0], 0))
    } else {
        SocketAddr::from(([0u16; 8], 0))
    };
    let std_socket =
        StdUdpSocket::bind(local).with_context(|| format!("绑定本地 UDP socket（{local}）失败"))?;
    std_socket
        .set_nonblocking(true)
        .context("把本地 UDP socket 设为非阻塞失败")?;

    // bind 阶段用 tokio 包装（recv 需要 await）；成功后 into_std 交回。
    let tokio_socket = TokioUdpSocket::from_std(std_socket).context("包装本地 UDP socket 失败")?;
    let next_nonce = Arc::new(AtomicU64::new(0));
    let (expires_in_s, server_ms) = wait_bind_ack(&tokio_socket, cfg, opts, &next_nonce).await?;
    let std_socket = tokio_socket
        .into_std()
        .context("归还本地 UDP socket 失败")?;

    // 保活用独立副本（同一 UDP 端口）：quinn 接管收包后保活只发不收。
    let keepalive_socket = std_socket
        .try_clone()
        .context("复制中继 UDP socket 用于保活失败")?;
    keepalive_socket
        .set_nonblocking(true)
        .context("把保活 UDP socket 设为非阻塞失败")?;
    let keepalive = spawn_keepalive(KeepaliveCtx {
        socket: keepalive_socket,
        relay_addr: cfg.relay_addr,
        tag: cfg.tag,
        role: cfg.role,
        key: cfg.key,
        nonce_seed: cfg.nonce,
        next_nonce: Arc::clone(&next_nonce),
        interval: opts.keepalive_interval,
    });

    tracing::trace!(
        relay = %cfg.relay_addr,
        role = %cfg.role,
        tag = %proto::tag_prefix(&cfg.tag),
        expires_in_s,
        "中继 bind 成功，保活任务已启动（{}s）",
        opts.keepalive_interval.as_secs()
    );

    Ok(RelaySocket {
        socket: std_socket,
        relay_addr: cfg.relay_addr,
        tag: cfg.tag,
        role: cfg.role,
        key: cfg.key,
        nonce_seed: cfg.nonce,
        next_nonce,
        expires_in_s,
        server_ms,
        keepalive: Some(keepalive),
    })
}

/// 生成第 `seq` 个 bind 使用的 16 字节 nonce：高 8B = 种子，低 8B = 序号。
fn bind_nonce(seed: u64, seq: u64) -> Nonce {
    let mut nonce = [0u8; proto::NONCE_LEN];
    nonce[..8].copy_from_slice(&seed.to_be_bytes());
    nonce[8..].copy_from_slice(&seq.to_be_bytes());
    nonce
}

/// 构造一条带 HMAC 的 BindRequest：
/// `auth = HMAC-SHA256(key, tag || role || nonce || ts_ms_be)`。
fn bind_request_packet(key: &[u8; 32], tag: &Tag, role: u8, nonce: &Nonce) -> Vec<u8> {
    let ts_ms = unix_ms();
    let auth = compute_auth(key, tag, role, nonce, ts_ms);
    proto::bind_packet(tag, role, nonce, ts_ms, &auth)
}

/// 解析 BindOk：`header(6) | tag(16) | expires_in_s(4 BE) | server_ms(8 BE)`。
fn parse_bind_ok(buf: &[u8]) -> Option<(Tag, u32, u64)> {
    if proto::classify(buf) != PacketClass::BindOk {
        return None;
    }
    let mut tag: Tag = [0u8; proto::TAG_LEN];
    tag.copy_from_slice(&buf[proto::HEADER_LEN..proto::HEADER_LEN + proto::TAG_LEN]);
    let body = proto::HEADER_LEN + proto::TAG_LEN;
    let expires_in_s = u32::from_be_bytes(buf[body..body + 4].try_into().ok()?);
    let server_ms = u64::from_be_bytes(buf[body + 4..body + 12].try_into().ok()?);
    Some((tag, expires_in_s, server_ms))
}

/// 解析 BindErr：`header(6) | tag(16) | code(1)`。
fn parse_bind_err(buf: &[u8]) -> Option<(Tag, u8)> {
    if proto::classify(buf) != PacketClass::BindErr {
        return None;
    }
    let mut tag: Tag = [0u8; proto::TAG_LEN];
    tag.copy_from_slice(&buf[proto::HEADER_LEN..proto::HEADER_LEN + proto::TAG_LEN]);
    let code = buf[proto::HEADER_LEN + proto::TAG_LEN];
    Some((tag, code))
}

/// 把中继拒绝码翻译成可执行的错误说明。
fn bind_err_message(code: u8) -> String {
    match code {
        proto::ERR_TAG_FULL => format!(
            "中继拒绝 bind（tag_full，code={code}）：该 tag 的此 role 槽位已被另一个地址占用，\
             旧端点仍在 rebind_guard 窗口内（默认 2s）。退避 2s 后重试，或给这一对端换新 tag"
        ),
        proto::ERR_BUSY => {
            format!("中继拒绝 bind（busy，code={code}）：中继会话数达到 max_sessions，稍后重试")
        }
        other => format!("中继拒绝 bind（code={other}）"),
    }
}

/// 发送 BindRequest 并等待 BindOk；超时按 retry_interval 重发（新 nonce）。
async fn wait_bind_ack(
    socket: &TokioUdpSocket,
    cfg: &RelayClientConfig,
    opts: &RelaySocketOptions,
    next_nonce: &AtomicU64,
) -> Result<(u32, u64)> {
    let mut buf = [0u8; 256];
    let deadline = Instant::now() + opts.bind_timeout;
    loop {
        let nonce = bind_nonce(cfg.nonce, next_nonce.fetch_add(1, Ordering::Relaxed));
        let pkt = bind_request_packet(&cfg.key, &cfg.tag, cfg.role.as_u8(), &nonce);
        socket
            .send_to(&pkt, cfg.relay_addr)
            .await
            .with_context(|| format!("向中继 {} 发送 BindRequest 失败", cfg.relay_addr))?;

        let now = Instant::now();
        if now >= deadline {
            bail!(
                "等待中继 {} 的 BindOk 超时（{}s）：请检查地址与 UDP 端口是否可达、\
                 中继 relay_key 与本端 key 是否一致（鉴权失败会被中继静默丢弃）",
                cfg.relay_addr,
                opts.bind_timeout.as_secs_f64()
            );
        }
        let wait = opts
            .retry_interval
            .min(deadline.saturating_duration_since(now));
        match tokio::time::timeout(wait, socket.recv_from(&mut buf)).await {
            // 本窗口内没回应：换新 nonce 重发。
            Err(_) => continue,
            Ok(Err(e)) => {
                return Err(e).with_context(|| {
                    format!("等待 BindOk 时 UDP 接收失败（中继 {}）", cfg.relay_addr)
                });
            }
            Ok(Ok((len, from))) => {
                // 只认中继地址的回包；bind 阶段不应收到数据面报文，忽略其他来源。
                if from != cfg.relay_addr {
                    continue;
                }
                let reply = &buf[..len];
                match proto::classify(reply) {
                    PacketClass::BindOk => {
                        let (tag, expires_in_s, server_ms) = parse_bind_ok(reply)
                            .ok_or_else(|| anyhow!("BindOk 字段非法（{len} 字节）"))?;
                        if tag != cfg.tag {
                            bail!(
                                "中继 BindOk 的 tag 不匹配：期望 {}，实际 {}",
                                proto::tag_prefix(&cfg.tag),
                                proto::tag_prefix(&tag)
                            );
                        }
                        return Ok((expires_in_s, server_ms));
                    }
                    PacketClass::BindErr => {
                        let (tag, code) = parse_bind_err(reply)
                            .ok_or_else(|| anyhow!("BindErr 字段非法（{len} 字节）"))?;
                        if tag != cfg.tag {
                            continue;
                        }
                        bail!("{}", bind_err_message(code));
                    }
                    // Bind/Bye/Data 出现在 bind 响应里都不合法：忽略并继续等。
                    _ => continue,
                }
            }
        }
    }
}

/// 保活任务上下文（避免 spawn 函数参数过多）。
struct KeepaliveCtx {
    socket: StdUdpSocket,
    relay_addr: SocketAddr,
    tag: Tag,
    role: Role,
    key: [u8; 32],
    nonce_seed: u64,
    next_nonce: Arc<AtomicU64>,
    interval: Duration,
}

/// 每 `interval` 重发一次 BindRequest（nonce 递增，保持中继会话不过期）。
fn spawn_keepalive(ctx: KeepaliveCtx) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(ctx.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // interval 的首个 tick 立即到期：先消费掉，让第一次保活在完整周期后发出。
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let nonce = bind_nonce(
                ctx.nonce_seed,
                ctx.next_nonce.fetch_add(1, Ordering::Relaxed),
            );
            let pkt = bind_request_packet(&ctx.key, &ctx.tag, ctx.role.as_u8(), &nonce);
            if let Err(e) = ctx.socket.send_to(&pkt, ctx.relay_addr) {
                if e.kind() != io::ErrorKind::WouldBlock {
                    tracing::warn!(
                        error = %e,
                        relay = %ctx.relay_addr,
                        "中继保活 BindRequest 发送失败"
                    );
                }
            }
        }
    })
}

#[cfg(test)]
mod client_tests {
    use super::*;

    #[test]
    fn role_matches_state_slots() {
        assert_eq!(Role::Agent.as_u8(), crate::state::ROLE_INIT);
        assert_eq!(Role::Client.as_u8(), crate::state::ROLE_RESP);
        assert_eq!(Role::from_u8(crate::state::ROLE_INIT), Some(Role::Agent));
        assert_eq!(Role::from_u8(crate::state::ROLE_RESP), Some(Role::Client));
        assert_eq!(Role::from_u8(2), None);
        assert_eq!("Agent".parse::<Role>().expect("合法角色").as_u8(), 0);
        assert_eq!("client".parse::<Role>().expect("合法角色").as_u8(), 1);
        assert!("other".parse::<Role>().is_err());
    }

    #[test]
    fn nonce_is_seeded_and_incrementing() {
        let a = bind_nonce(7, 0);
        let b = bind_nonce(7, 1);
        let c = bind_nonce(8, 0);
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_eq!(&a[..8], &7u64.to_be_bytes());
        assert_eq!(&a[8..], &0u64.to_be_bytes());
        assert_eq!(&b[8..], &1u64.to_be_bytes());
    }

    #[test]
    fn bind_ok_roundtrip() {
        let tag = [0x11u8; proto::TAG_LEN];
        let pkt = proto::bind_ok_packet(&tag, 3600, 1_234);
        assert_eq!(parse_bind_ok(&pkt), Some((tag, 3600, 1_234)));
    }

    #[test]
    fn bind_err_roundtrip() {
        let tag = [0x22u8; proto::TAG_LEN];
        let pkt = proto::bind_err_packet(&tag, proto::ERR_TAG_FULL);
        assert_eq!(parse_bind_err(&pkt), Some((tag, proto::ERR_TAG_FULL)));
        assert!(bind_err_message(proto::ERR_TAG_FULL).contains("tag_full"));
        assert!(bind_err_message(proto::ERR_BUSY).contains("busy"));
    }

    #[test]
    fn junk_is_not_an_ack() {
        assert!(parse_bind_ok(&[]).is_none());
        assert!(parse_bind_ok(&[0u8; proto::BIND_OK_LEN]).is_none());
        assert!(parse_bind_err(&[0u8; proto::BIND_ERR_LEN]).is_none());
        // 正确头部但长度不对 → classify 判 Malformed，解析必须拒绝。
        let mut pkt = proto::bind_ok_packet(&[0u8; proto::TAG_LEN], 1, 2);
        pkt.push(0);
        assert!(parse_bind_ok(&pkt).is_none());
    }
}
