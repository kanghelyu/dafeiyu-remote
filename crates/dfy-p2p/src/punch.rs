// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 双端并发 UDP 打洞（P2P punch）。
//!
//! 流程：双方各自用自己的 socket 以固定间隔向对端候选发 `PROBE` 帧；收到对方 `PROBE`
//! 说明路径已通，立即回 `ACK` 并视为成功；收到 cookie 匹配的 `ACK` 也视为成功。
//! 成功后把同一个非阻塞 `std::net::UdpSocket` 连同对端真实地址返回，直接交给
//! `dfy-transport` 的 quinn `Endpoint`（见 `design/023-p2p-punch.md` §6）。
//!
//! ## 打洞帧（13 字节，刻意做得不像任何已知协议）
//!
//! ```text
//! | magic (4) | kind (1) | cookie (8) |
//! ```
//!
//! - `magic`：双方约定的 4 字节魔数（默认 `DFYP`），用于把打洞帧与 QUIC/其他流量区分开；
//! - `kind`：1 = PROBE，2 = ACK；
//! - `cookie`：发送方随机 8 字节；`ACK` 必须原样回带收到的 `PROBE` 的 cookie，
//!   避免把陈旧包/伪造包当成打洞成功。
//!
//! 打洞阶段的残包（对端回给我们的 `ACK`）在进入 QUIC 后会被 quinn 当作非 QUIC
//! 报文丢弃，不影响握手；自测里则显式 drain 后再互发业务数据。

use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::net::UdpSocket as TokioUdpSocket;
use tokio::time::Instant;
use tracing::{debug, trace, warn};

/// 打洞帧字节数。
pub const PUNCH_FRAME_LEN: usize = 13;
/// 默认打洞魔数（`"DFYP"` = DaFeiYu Punch）。
pub const PUNCH_MAGIC: [u8; 4] = *b"DFYP";
/// `PROBE` 帧类型。
pub const PUNCH_KIND_PROBE: u8 = 1;
/// `ACK` 帧类型。
pub const PUNCH_KIND_ACK: u8 = 2;
/// cookie 字节数。
pub const PUNCH_COOKIE_LEN: usize = 8;
/// 默认打洞总超时（对齐 `design/013-relay-and-p2p.md` 的 1.5s 打洞预算）。
pub const DEFAULT_PUNCH_DEADLINE: Duration = Duration::from_millis(1500);
/// 默认发包间隔。
pub const DEFAULT_PUNCH_INTERVAL: Duration = Duration::from_millis(100);

/// 一个打洞帧。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PunchFrame {
    /// 4 字节魔数。
    pub magic: [u8; 4],
    /// 帧类型（[`PUNCH_KIND_PROBE`] / [`PUNCH_KIND_ACK`]）。
    pub kind: u8,
    /// 8 字节随机 cookie。
    pub cookie: [u8; PUNCH_COOKIE_LEN],
}

impl PunchFrame {
    /// 构造 `PROBE` 帧。
    pub fn probe(magic: [u8; 4], cookie: [u8; PUNCH_COOKIE_LEN]) -> Self {
        Self {
            magic,
            kind: PUNCH_KIND_PROBE,
            cookie,
        }
    }

    /// 构造 `ACK` 帧（回带对端 `PROBE` 的 cookie）。
    pub fn ack(magic: [u8; 4], cookie: [u8; PUNCH_COOKIE_LEN]) -> Self {
        Self {
            magic,
            kind: PUNCH_KIND_ACK,
            cookie,
        }
    }

    /// 是否是 `PROBE`。
    pub fn is_probe(&self) -> bool {
        self.kind == PUNCH_KIND_PROBE
    }

    /// 是否是 `ACK`。
    pub fn is_ack(&self) -> bool {
        self.kind == PUNCH_KIND_ACK
    }

    /// 编码成 13 字节。
    pub fn encode(&self) -> [u8; PUNCH_FRAME_LEN] {
        let mut bytes = [0u8; PUNCH_FRAME_LEN];
        bytes[..4].copy_from_slice(&self.magic);
        bytes[4] = self.kind;
        bytes[5..].copy_from_slice(&self.cookie);
        bytes
    }

    /// 解码；长度不是 13 字节时返回 `None`（QUIC/其它流量一律忽略）。
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != PUNCH_FRAME_LEN {
            return None;
        }
        let mut magic = [0u8; 4];
        magic.copy_from_slice(&bytes[..4]);
        let mut cookie = [0u8; PUNCH_COOKIE_LEN];
        cookie.copy_from_slice(&bytes[5..]);
        Some(Self {
            magic,
            kind: bytes[4],
            cookie,
        })
    }
}

/// 打洞参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PunchOptions {
    /// 打洞总超时；超时后返回错误，由调用方回退 relay。
    pub deadline: Duration,
    /// 向候选发包的间隔。
    pub interval: Duration,
    /// 双方约定的帧魔数。
    pub magic: [u8; 4],
}

impl Default for PunchOptions {
    fn default() -> Self {
        Self {
            deadline: DEFAULT_PUNCH_DEADLINE,
            interval: DEFAULT_PUNCH_INTERVAL,
            magic: PUNCH_MAGIC,
        }
    }
}

impl PunchOptions {
    /// 默认参数。
    pub fn new() -> Self {
        Self::default()
    }

    /// 修改总超时。
    pub fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }

    /// 修改发包间隔。
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// 修改魔数（信令层可派生每次会话不同的值）。
    pub fn with_magic(mut self, magic: [u8; 4]) -> Self {
        self.magic = magic;
        self
    }
}

/// 打洞成功后的会话：已连通的 UDP socket + 对端真实地址，可直接交给 quinn。
#[derive(Debug)]
pub struct P2PSession {
    /// 打洞/STUN 使用的同一个 socket（非阻塞），交给 quinn 复用。
    pub socket: UdpSocket,
    /// 实际收到对端打洞帧的地址（NAT 映射后的公网地址，非候选表里的地址）。
    pub peer: SocketAddr,
}

impl P2PSession {
    /// 本地 socket 地址。
    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.socket
            .local_addr()
            .context("读取 P2P socket 本地地址失败")
    }

    /// 拆出 socket 与对端地址。
    pub fn into_parts(self) -> (UdpSocket, SocketAddr) {
        (self.socket, self.peer)
    }

    /// 复制 socket 句柄（共享同一 UDP 端口）用于探测/存活检测。
    pub fn try_clone_socket(&self) -> Result<UdpSocket> {
        self.socket.try_clone().context("复制 P2P socket 句柄失败")
    }
}

/// 双端并发打洞。
///
/// `local` 必须是收集候选（含 STUN 探测）时用的同一个 socket，否则映射地址不一致、
/// 打洞必然失败。返回的 [`P2PSession::socket`] 保持非阻塞，可直接 `quinn::Endpoint::new`。
pub async fn punch(
    local: UdpSocket,
    remote_candidates: &[SocketAddr],
    opts: PunchOptions,
) -> Result<P2PSession> {
    if remote_candidates.is_empty() {
        anyhow::bail!("远端候选地址为空，无法打洞（应直接回退 relay）");
    }
    if opts.deadline.is_zero() {
        anyhow::bail!("打洞超时必须大于 0");
    }
    if opts.interval.is_zero() {
        anyhow::bail!("打洞发包间隔必须大于 0");
    }

    local
        .set_nonblocking(true)
        .context("把打洞 socket 设为非阻塞失败")?;
    let local_addr = local.local_addr().context("读取打洞 socket 本地地址失败")?;
    debug!(
        %local_addr,
        candidates = remote_candidates.len(),
        deadline_ms = opts.deadline.as_secs_f64() * 1000.0,
        "开始 UDP 打洞"
    );

    let socket = TokioUdpSocket::from_std(local).context("包装打洞 socket 失败")?;
    let (socket, peer) = run_punch(socket, remote_candidates, opts).await?;
    let socket = socket.into_std().context("还原打洞 socket 失败")?;
    debug!(%local_addr, %peer, "UDP 打洞成功，socket 已连通");
    Ok(P2PSession { socket, peer })
}

async fn run_punch(
    socket: TokioUdpSocket,
    remote_candidates: &[SocketAddr],
    opts: PunchOptions,
) -> Result<(TokioUdpSocket, SocketAddr)> {
    let cookie: [u8; PUNCH_COOKIE_LEN] = rand::random();
    let probe = PunchFrame::probe(opts.magic, cookie).encode();
    let started = Instant::now();
    let deadline = started + opts.deadline;
    let mut buffer = [0u8; 1500];

    while Instant::now() < deadline {
        // 一轮：向所有候选各发一个 PROBE（单个候选失败不致命，可能只是路由不可达）。
        for remote in remote_candidates {
            if let Err(error) = socket.send_to(&probe, *remote).await {
                trace!(%remote, %error, "发送打洞 PROBE 失败（继续尝试其它候选）");
            }
        }

        // 本轮接收窗口：至少覆盖母公司 interval，但不超过总 deadline。
        let window_end = (Instant::now() + opts.interval).min(deadline);
        loop {
            let remaining = window_end.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, socket.recv_from(&mut buffer)).await {
                Err(_elapsed) => break,
                Ok(Err(error)) => {
                    // Windows 上对端端口未监听会因 ICMP 让 recv 返回 ConnectionReset，
                    // 属于打洞期间正常现象，继续下一轮即可。
                    if is_transient_recv_error(&error) {
                        trace!(%error, "打洞接收遇到可忽略的 socket 错误");
                        continue;
                    }
                    return Err(anyhow::Error::new(error).context("打洞时接收 UDP 包失败"));
                }
                Ok(Ok((len, source))) => {
                    let Some(frame) = PunchFrame::decode(&buffer[..len]) else {
                        trace!(%source, len, "忽略非打洞帧");
                        continue;
                    };
                    if frame.magic != opts.magic {
                        trace!(%source, "忽略魔数不匹配的打洞帧");
                        continue;
                    }
                    if frame.is_probe() {
                        // 收到对端 PROBE = 路径双向可达（我们收到了它的包）。
                        // 先尽量回 ACK，让对端也能尽快成功（对端 PROBE 可能全丢）。
                        let ack = PunchFrame::ack(opts.magic, frame.cookie).encode();
                        if let Err(error) = socket.send_to(&ack, source).await {
                            trace!(%source, %error, "回打洞 ACK 失败（对方可能靠自己的 ACK 成功）");
                        }
                        return Ok((socket, source));
                    }
                    if frame.is_ack() && frame.cookie == cookie {
                        return Ok((socket, source));
                    }
                    trace!(%source, kind = frame.kind, "忽略无关/陈旧打洞帧");
                }
            }
        }

        // 最后 10ms 已不可能再发一轮，直接收尾。
        if deadline.saturating_duration_since(Instant::now()) < opts.interval / 2 {
            break;
        }
    }

    warn!(
        candidates = remote_candidates.len(),
        elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
        "UDP 打洞超时：所有候选均未收到对端打洞帧，应回退 relay"
    );
    anyhow::bail!(
        "UDP 打洞超时（{:?}，候选 {} 个），应回退 relay",
        started.elapsed(),
        remote_candidates.len()
    )
}

/// 打洞期间可忽略的 socket 错误（ICMP 反馈、非阻塞无数据等）。
fn is_transient_recv_error(error: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    matches!(
        error.kind(),
        ErrorKind::ConnectionReset
            | ErrorKind::ConnectionRefused
            | ErrorKind::ConnectionAborted
            | ErrorKind::WouldBlock
            | ErrorKind::TimedOut
            | ErrorKind::Interrupted
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddrV4};

    /// 排空打洞阶段残留的 PROBE/ACK（连续 `idle` 无包即停）。
    async fn drain_punch_frames(socket: &TokioUdpSocket, idle: Duration) {
        let mut buffer = [0u8; 1500];
        loop {
            match tokio::time::timeout(idle, socket.recv_from(&mut buffer)).await {
                Ok(Ok(_)) => continue,
                _ => return,
            }
        }
    }

    fn loopback_addr() -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
    }

    fn bind_loopback() -> UdpSocket {
        UdpSocket::bind(loopback_addr()).expect("绑定回环 UDP socket 成功")
    }

    #[test]
    fn frame_encode_decode_round_trip() {
        let cookie = *b"12345678";
        let probe = PunchFrame::probe(PUNCH_MAGIC, cookie);
        assert_eq!(PunchFrame::decode(&probe.encode()), Some(probe));
        assert!(probe.is_probe() && !probe.is_ack());

        let ack = PunchFrame::ack(PUNCH_MAGIC, cookie);
        assert_eq!(PunchFrame::decode(&ack.encode()), Some(ack));
        assert!(ack.is_ack() && !ack.is_probe());
    }

    #[test]
    fn frame_decode_rejects_short_or_misaligned_packets() {
        assert_eq!(PunchFrame::decode(&[]), None);
        assert_eq!(PunchFrame::decode(&[0u8; PUNCH_FRAME_LEN - 1]), None);
        assert_eq!(PunchFrame::decode(&[0u8; PUNCH_FRAME_LEN + 1]), None);
    }

    #[test]
    fn options_builder_overrides_defaults() {
        let opts = PunchOptions::new()
            .with_deadline(Duration::from_millis(300))
            .with_interval(Duration::from_millis(20))
            .with_magic(*b"ABCD");
        assert_eq!(opts.deadline, Duration::from_millis(300));
        assert_eq!(opts.interval, Duration::from_millis(20));
        assert_eq!(opts.magic, *b"ABCD");
        assert_eq!(PunchOptions::default(), PunchOptions::new());
    }

    #[tokio::test]
    async fn punch_rejects_empty_candidates() {
        let socket = bind_loopback();
        let error = punch(socket, &[], PunchOptions::default())
            .await
            .expect_err("空候选必须报错");
        assert!(error.to_string().contains("远端候选"));
    }

    #[tokio::test]
    async fn punch_rejects_zero_deadline() {
        let socket = bind_loopback();
        let opts = PunchOptions::default().with_deadline(Duration::ZERO);
        let error = punch(socket, &[loopback_addr()], opts)
            .await
            .expect_err("零超时必须报错");
        assert!(error.to_string().contains("超时"));
    }

    #[tokio::test]
    async fn two_loopback_sockets_punch_and_exchange_data() {
        let socket_a = bind_loopback();
        let socket_b = bind_loopback();
        let addr_a = socket_a.local_addr().expect("读取 A 地址");
        let addr_b = socket_b.local_addr().expect("读取 B 地址");
        let opts = PunchOptions::default()
            .with_deadline(Duration::from_secs(1))
            .with_interval(Duration::from_millis(20));

        let candidates_a = [addr_b];
        let candidates_b = [addr_a];
        let (session_a, session_b) = tokio::join!(
            punch(socket_a, &candidates_a, opts),
            punch(socket_b, &candidates_b, opts),
        );
        let session_a = session_a.expect("A 端打洞应成功");
        let session_b = session_b.expect("B 端打洞应成功");
        assert_eq!(session_a.peer, addr_b, "A 看到的对端应是 B 的本地地址");
        assert_eq!(session_b.peer, addr_a, "B 看到的对端应是 A 的本地地址");
        assert_eq!(
            session_a.local_addr().expect("A 本地地址").ip(),
            Ipv4Addr::LOCALHOST
        );

        // 打洞阶段互回的 ACK 可能还留在接收队列里，先排空再验证业务数据互通。
        let socket_a =
            TokioUdpSocket::from_std(session_a.into_parts().0).expect("A 转 tokio socket");
        let socket_b =
            TokioUdpSocket::from_std(session_b.into_parts().0).expect("B 转 tokio socket");
        drain_punch_frames(&socket_a, Duration::from_millis(50)).await;
        drain_punch_frames(&socket_b, Duration::from_millis(50)).await;

        socket_a
            .send_to(b"ping-from-a", addr_b)
            .await
            .expect("A 发送数据");
        let mut buffer = [0u8; 64];
        let (len, from) =
            tokio::time::timeout(Duration::from_secs(1), socket_b.recv_from(&mut buffer))
                .await
                .expect("B 应在一秒内收到数据")
                .expect("B 接收数据失败");
        assert_eq!(&buffer[..len], b"ping-from-a");
        assert_eq!(from, addr_a);

        socket_b
            .send_to(b"pong-from-b", addr_a)
            .await
            .expect("B 回发数据");
        let (len, from) =
            tokio::time::timeout(Duration::from_secs(1), socket_a.recv_from(&mut buffer))
                .await
                .expect("A 应在一秒内收到回包")
                .expect("A 接收回包失败");
        assert_eq!(&buffer[..len], b"pong-from-b");
        assert_eq!(from, addr_b);
    }

    #[tokio::test]
    async fn punch_times_out_when_peer_silent() {
        // 对端 socket 只接收、从不回帧：模拟单向可达 / 对端未打洞。
        let silent_peer = bind_loopback();
        let silent_addr = silent_peer.local_addr().expect("读取静默对端地址");
        let socket = bind_loopback();
        let opts = PunchOptions::default()
            .with_deadline(Duration::from_millis(300))
            .with_interval(Duration::from_millis(30));

        let started = std::time::Instant::now();
        let error = punch(socket, &[silent_addr], opts)
            .await
            .expect_err("对端静默时打洞必须超时");
        let elapsed = started.elapsed();
        assert!(error.to_string().contains("打洞超时"));
        assert!(
            elapsed >= Duration::from_millis(250),
            "实际耗时 {elapsed:?} 应接近 deadline"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "不应远超 deadline：{elapsed:?}"
        );
    }

    #[tokio::test]
    async fn punch_succeeds_from_ack_when_peer_probe_was_lost() {
        // 假对端只回 ACK、从不发 PROBE：本端唯一可能的成功路径就是"匹配 cookie 的 ACK"。
        let stub = bind_loopback();
        let stub_addr = stub.local_addr().expect("读取 stub 地址");
        let socket = bind_loopback();
        stub.set_nonblocking(true).expect("stub 设非阻塞");
        let stub = TokioUdpSocket::from_std(stub).expect("stub 转 tokio socket");

        let opts = PunchOptions::default()
            .with_deadline(Duration::from_secs(1))
            .with_interval(Duration::from_millis(20));

        let stub_task = async move {
            let mut buffer = [0u8; 64];
            loop {
                let received =
                    tokio::time::timeout(Duration::from_millis(300), stub.recv_from(&mut buffer))
                        .await;
                let Ok(Ok((len, from))) = received else {
                    return;
                };
                let Some(frame) = PunchFrame::decode(&buffer[..len]) else {
                    continue;
                };
                if frame.is_probe() {
                    let ack = PunchFrame::ack(opts.magic, frame.cookie).encode();
                    stub.send_to(&ack, from).await.expect("stub 回 ACK");
                    return;
                }
            }
        };

        let candidates = [stub_addr];
        let (session, ()) = tokio::join!(punch(socket, &candidates, opts), stub_task);
        let session = session.expect("收到匹配 ACK 应打洞成功");
        assert_eq!(session.peer, stub_addr);
    }
}
