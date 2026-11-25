// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Dafeiyu Remote contributors

//! Client（主控端）会话：接收视频 DATAGRAM → 分片重组 → 解码 → 交给 UI/回调；
//! 同时维护 control 流心跳。

use std::time::{Duration, Instant};

use anyhow::Result;
use dfy_proto::{decode_video, envelope, Envelope, Welcome};
use dfy_transport::{CtrlChannel, Heartbeat};
use quinn::Connection;
use tokio::time::MissedTickBehavior;
use tracing::{debug, info};

use crate::{DecodedFrame, Decoder, FrameAssembler};

/// Client 运行参数。
#[derive(Clone, Debug)]
pub struct ClientConfig {
    /// 无任何收发活动超过该时长即结束会话。
    pub idle_timeout: Duration,
    /// 期望的帧数（到达后正常结束；`None` = 一直收，直到对端关闭）。
    pub expect_frames: Option<u64>,
    /// 心跳发送间隔。
    pub ping_interval: Duration,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            idle_timeout: Duration::from_secs(3600),
            expect_frames: None,
            ping_interval: dfy_transport::PING_INTERVAL,
        }
    }
}

impl ClientConfig {
    /// 收够 `n` 帧即结束（selftest/e2e 用）。
    pub fn with_expect_frames(mut self, n: u64) -> Self {
        self.expect_frames = Some(n);
        self
    }

    /// 自定义心跳间隔（测试里缩短，便于快速测到 RTT）。
    pub fn with_ping_interval(mut self, interval: Duration) -> Self {
        self.ping_interval = interval;
        self
    }
}

/// Client 侧一次会话的统计。
#[derive(Clone, Debug, Default)]
pub struct ClientRunStats {
    /// 收到的视频 DATAGRAM 包数。
    pub packets_recv: u64,
    /// 收到的视频负载字节。
    pub bytes_recv: u64,
    /// 重组完成的帧数。
    pub frames_assembled: u64,
    /// 成功解码并交付的帧数。
    pub frames_delivered: u64,
    /// 关键帧数。
    pub keyframes: u64,
    /// 解析失败的 DATAGRAM 数。
    pub malformed: u64,
    /// 因内存上限被丢弃的在途帧数。
    pub dropped_frames: u64,
    /// 最近一次 RTT（毫秒）。
    pub rtt_ms: Option<f64>,
    /// 结束原因。
    pub end_reason: &'static str,
}

/// 跑一次 Client 会话，把解码后的帧交给 `on_frame`。
pub async fn run_client(
    conn: Connection,
    welcome: Welcome,
    chan: CtrlChannel,
    mut decoder: impl Decoder,
    mut on_frame: impl FnMut(DecodedFrame),
    cfg: ClientConfig,
) -> Result<ClientRunStats> {
    let (mut tx, mut rx) = chan.split();
    let mut heartbeat = Heartbeat::new();
    heartbeat.interval = cfg.ping_interval;
    let mut assembler = FrameAssembler::new();
    let mut stats = ClientRunStats::default();
    let mut frames_left = cfg.expect_frames;
    let mut last_activity = Instant::now();
    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    info!(
        agent = %welcome.agent_name,
        session = %hex::encode(&welcome.session_id),
        expect_frames = ?frames_left,
        "Client 会话开始"
    );

    loop {
        tokio::select! {
            biased;

            _ = ticker.tick() => {
                let now = Instant::now();
                if now >= heartbeat.deadline() {
                    let ts = dfy_transport::now_us();
                    heartbeat.mark_ping_sent(ts);
                    if let Err(err) = tx.send_ctrl(&Envelope::ping(ts)).await {
                        debug!(error = %err, "发送 Ping 失败");
                        stats.end_reason = "ctrl_closed";
                        break;
                    }
                }
                if heartbeat.timed_out() {
                    stats.end_reason = "heartbeat_timeout";
                    break;
                }
                if last_activity.elapsed() > cfg.idle_timeout {
                    stats.end_reason = "idle_timeout";
                    break;
                }
                if frames_left == Some(0) {
                    stats.end_reason = "expect_frames";
                    break;
                }
            }

            incoming = rx.recv_ctrl() => {
                match incoming {
                    Ok(None) => {
                        stats.end_reason = "peer_closed";
                        break;
                    }
                    Err(err) => {
                        debug!(error = %err, "读取 control 帧失败");
                        stats.end_reason = "ctrl_closed";
                        break;
                    }
                    Ok(Some(env)) => {
                        last_activity = Instant::now();
                        if let Some(reply) = heartbeat.on_frame(&env) {
                            if let Err(err) = tx.send_ctrl(&reply).await {
                                debug!(error = %err, "回复 Pong 失败");
                            }
                        }
                        match env.body {
                            Some(envelope::Body::Bye(bye)) => {
                                stats.end_reason = if bye.reason == dfy_proto::BYE_NORMAL {
                                    "agent_bye"
                                } else {
                                    "agent_bye_user"
                                };
                                break;
                            }
                            Some(envelope::Body::Reject(_)) => {
                                stats.end_reason = "rejected";
                                break;
                            }
                            _ => {}
                        }
                    }
                }
            }

            datagram = conn.read_datagram() => {
                match datagram {
                    Ok(bytes) => {
                        last_activity = Instant::now();
                        match decode_video(&bytes) {
                            Ok((header, payload)) => {
                                stats.packets_recv += 1;
                                stats.bytes_recv += payload.len() as u64;
                                if header.is_key() {
                                    stats.keyframes += 1;
                                }
                                match assembler.push(&header, payload) {
                                    Ok(Some(complete)) => {
                                        stats.frames_assembled += 1;
                                        match decoder.decode(&complete.data, u64::from(complete.ts_us)) {
                                            Ok(Some(frame)) => {
                                                stats.frames_delivered += 1;
                                                on_frame(frame);
                                                frames_left = frames_left.map(|n| n.saturating_sub(1));
                                            }
                                            Ok(None) => {}
                                            Err(err) => {
                                                debug!(error = %err, frame_id = complete.frame_id, "解码失败");
                                            }
                                        }
                                    }
                                    Ok(None) => {}
                                    Err(err) => {
                                        debug!(error = %err, "重组失败");
                                    }
                                }
                            }
                            Err(err) => {
                                stats.malformed += 1;
                                debug!(error = %err, "解析视频 DATAGRAM 失败");
                            }
                        }
                    }
                    Err(err) => {
                        debug!(error = %err, "读取 DATAGRAM 失败（连接关闭）");
                        stats.end_reason = "conn_closed";
                        break;
                    }
                }
            }
        }
    }

    stats.rtt_ms = heartbeat.rtt_ms();
    stats.dropped_frames = assembler.dropped();
    let _ = tx.send_ctrl(&Envelope::bye(dfy_proto::BYE_NORMAL)).await;
    tx.finish().ok();
    conn.close(0u32.into(), b"done");
    info!(
        frames = stats.frames_delivered,
        bytes = stats.bytes_recv,
        rtt_ms = ?stats.rtt_ms,
        reason = stats.end_reason,
        "Client 会话结束"
    );
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_config_builder() {
        let cfg = ClientConfig::default().with_expect_frames(8);
        assert_eq!(cfg.expect_frames, Some(8));
        assert!(cfg.idle_timeout >= Duration::from_secs(60));
    }
}
