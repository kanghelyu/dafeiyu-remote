//! 会话封装 + 心跳状态机。
//!
//! 这里刻意不隐藏 control 流的收/发句柄：调用方（demo / selftest）自己用
//! `tokio::select!` 组合「按 5s 发 Ping」与「读消息」，同时把视频/输入 DATAGRAM
//! 的发送节奏放在自己的 tick 里。`Heartbeat` 只做纯状态计算，便于单测。

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use dfy_proto::{envelope, Envelope, Welcome};
use quinn::{Connection, VarInt};
use tracing::{debug, warn};

use crate::{ctrl::CtrlChannel, util::now_us};

/// 心跳间隔（对应 spec：5s）。
pub const PING_INTERVAL: Duration = Duration::from_secs(5);
/// 心跳超时（对应 spec：15s 未收到 Pong 即断开）。
pub const PING_TIMEOUT: Duration = Duration::from_secs(15);

/// 心跳状态机：纯计算，无 IO。
#[derive(Debug)]
pub struct Heartbeat {
    /// 发送间隔。
    pub interval: Duration,
    /// 超时阈值。
    pub timeout: Duration,
    next_ping_at: Instant,
    pending: Option<(u64, Instant)>,
    /// 最近一次 RTT（微秒）。
    pub last_rtt_us: Option<u64>,
    /// 已发送 Ping 数。
    pub pings_sent: u64,
    /// 已收到 Pong 数。
    pub pongs_recv: u64,
}

impl Default for Heartbeat {
    fn default() -> Self {
        Self::new()
    }
}

impl Heartbeat {
    /// 创建心跳状态机：**立即**发第一个 Ping（demo 时长 1–3s 内也能测到 RTT）。
    pub fn new() -> Self {
        Self {
            interval: PING_INTERVAL,
            timeout: PING_TIMEOUT,
            next_ping_at: Instant::now(),
            pending: None,
            last_rtt_us: None,
            pings_sent: 0,
            pongs_recv: 0,
        }
    }

    /// 下一个 Ping 的到期时刻（配合 `tokio::time::sleep_until`）。
    pub fn deadline(&self) -> Instant {
        self.next_ping_at
    }

    /// 记录「刚发出一个 Ping」。
    pub fn mark_ping_sent(&mut self, ts_us: u64) {
        self.pings_sent += 1;
        self.pending = Some((ts_us, Instant::now()));
        self.next_ping_at = Instant::now() + self.interval;
    }

    /// 处理收到的 control 消息：`Ping` → 返回应回的 `Pong`；`Pong` → 更新 RTT。
    pub fn on_frame(&mut self, envelope: &Envelope) -> Option<Envelope> {
        match &envelope.body {
            Some(envelope::Body::Ping(ping)) => Some(Envelope::pong(ping.ts_us)),
            Some(envelope::Body::Pong(pong)) => {
                // 用回显 ts_us 计算 RTT，避免两端时钟漂移影响
                let rtt = now_us().saturating_sub(pong.ts_us);
                self.last_rtt_us = Some(rtt);
                self.pongs_recv += 1;
                if let Some((sent_ts, _)) = self.pending.take() {
                    if sent_ts != pong.ts_us {
                        debug!(
                            sent = sent_ts,
                            echo = pong.ts_us,
                            "Pong 回显时间戳与最近一次 Ping 不一致"
                        );
                    }
                }
                None
            }
            _ => None,
        }
    }

    /// 最近一次 RTT（毫秒，保留两位）。
    pub fn rtt_ms(&self) -> Option<f64> {
        self.last_rtt_us.map(|us| us as f64 / 1000.0)
    }

    /// 是否已超过 15s 未收到 Pong。
    pub fn timed_out(&self) -> bool {
        self.pending
            .as_ref()
            .map(|(_, sent_at)| sent_at.elapsed() > self.timeout)
            .unwrap_or(false)
    }
}

/// Agent 侧会话：已握手 + control 通道句柄。
pub struct AgentSession {
    /// QUIC 连接。
    pub conn: Connection,
    /// 握手返回的 Welcome。
    pub welcome: Welcome,
    /// control 发送句柄。
    pub tx: crate::ctrl::CtrlSender,
    /// control 接收句柄。
    pub rx: crate::ctrl::CtrlReceiver,
    /// 心跳状态。
    pub heartbeat: Heartbeat,
}

/// Client 侧会话：已握手 + control 通道句柄。
pub struct ClientSession {
    /// QUIC 连接。
    pub conn: Connection,
    /// 握手返回的 Welcome。
    pub welcome: Welcome,
    /// control 发送句柄。
    pub tx: crate::ctrl::CtrlSender,
    /// control 接收句柄。
    pub rx: crate::ctrl::CtrlReceiver,
    /// 心跳状态。
    pub heartbeat: Heartbeat,
}

impl AgentSession {
    /// 由握手结果组装会话。
    pub fn new(conn: Connection, welcome: Welcome, channel: CtrlChannel) -> Self {
        let (tx, rx) = channel.split();
        Self {
            conn,
            welcome,
            tx,
            rx,
            heartbeat: Heartbeat::new(),
        }
    }

    /// 发送一个 control 帧。
    pub async fn send_ctrl(&mut self, envelope: &Envelope) -> Result<()> {
        self.tx.send_ctrl(envelope).await
    }

    /// 读取一个 control 帧（`None` = 对端关闭）。
    pub async fn recv_ctrl(&mut self) -> Result<Option<Envelope>> {
        self.rx.recv_ctrl().await
    }

    /// 关闭会话：发 Bye 后关闭 QUIC 连接。
    pub async fn bye(mut self, reason: u32) -> Result<()> {
        let _ = self.tx.send_ctrl(&Envelope::bye(reason)).await;
        self.tx.finish().ok();
        self.conn.close(VarInt::from_u32(0), b"bye");
        Ok(())
    }
}

impl ClientSession {
    /// 由握手结果组装会话。
    pub fn new(conn: Connection, welcome: Welcome, channel: CtrlChannel) -> Self {
        let (tx, rx) = channel.split();
        Self {
            conn,
            welcome,
            tx,
            rx,
            heartbeat: Heartbeat::new(),
        }
    }

    /// 发送一个 control 帧。
    pub async fn send_ctrl(&mut self, envelope: &Envelope) -> Result<()> {
        self.tx.send_ctrl(envelope).await
    }

    /// 读取一个 control 帧（`None` = 对端关闭）。
    pub async fn recv_ctrl(&mut self) -> Result<Option<Envelope>> {
        self.rx.recv_ctrl().await
    }

    /// 立即测一次 RTT：发 Ping、等 Pong（超时 3s）。
    pub async fn ping_once(&mut self) -> Result<u64> {
        let ts = now_us();
        self.tx
            .send_ctrl(&Envelope::ping(ts))
            .await
            .context("发送 Ping 失败")?;
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            anyhow::ensure!(!remaining.is_zero(), "等待 Pong 超时（3s）");
            match tokio::time::timeout(remaining, self.rx.recv_ctrl()).await {
                Ok(Ok(Some(env))) => {
                    if matches!(env.body, Some(envelope::Body::Pong(_))) {
                        let rtt = now_us().saturating_sub(ts);
                        self.heartbeat.on_frame(&env);
                        return Ok(rtt);
                    }
                    warn!(
                        kind = env.kind_name(),
                        "等待 Pong 期间收到其他消息（已忽略）"
                    );
                }
                Ok(Ok(None)) => anyhow::bail!("等待 Pong 前连接已关闭"),
                Ok(Err(e)) => return Err(e),
                Err(_) => anyhow::bail!("等待 Pong 超时（3s）"),
            }
        }
    }

    /// 发送 Bye 并关闭连接。
    pub async fn bye(mut self, reason: u32) -> Result<()> {
        let _ = self.tx.send_ctrl(&Envelope::bye(reason)).await;
        self.tx.finish().ok();
        // 给 Bye 一点时间落地（不可靠网络的善意行为；正式版可等对端 Close）
        tokio::time::sleep(Duration::from_millis(50)).await;
        self.conn.close(VarInt::from_u32(0), b"bye");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heartbeat_replies_pong_and_tracks_rtt() {
        let mut hb = Heartbeat::new();
        assert!(hb.deadline() <= Instant::now());
        hb.mark_ping_sent(1_000);
        assert_eq!(hb.pings_sent, 1);
        assert!(!hb.timed_out());

        // 对端 Ping → 我们要回 Pong（回显同一个 ts_us）
        let reply = hb.on_frame(&Envelope::ping(777)).expect("应回 Pong");
        match reply.body {
            Some(envelope::Body::Pong(pong)) => assert_eq!(pong.ts_us, 777),
            other => panic!("期望 Pong, 实际 {other:?}"),
        }

        // 自己的 Pong 回来 → RTT 有值、pending 清空
        let sent = now_us().saturating_sub(1_500);
        hb.mark_ping_sent(sent);
        assert!(hb.on_frame(&Envelope::pong(sent)).is_none());
        assert!(hb.last_rtt_us.unwrap() >= 1_000);
        assert_eq!(hb.pongs_recv, 1);
        assert!(!hb.timed_out());
    }

    #[test]
    fn heartbeat_defaults_to_5s_and_15s() {
        let hb = Heartbeat::new();
        assert_eq!(hb.interval, Duration::from_secs(5));
        assert_eq!(hb.timeout, Duration::from_secs(15));
    }
}
