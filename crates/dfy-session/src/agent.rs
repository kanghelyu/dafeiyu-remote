// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Dafeiyu Remote contributors

//! Agent（被控端）会话：把采集线程产出的码流持续发成视频 DATAGRAM，同时处理 control 流
//! （心跳、Bye、自定义消息）与客户端发来的 input-move DATAGRAM。

use std::time::{Duration, Instant};

use anyhow::Result;
use dfy_proto::{envelope, Envelope, Welcome, CHAN_INPUT_MOVE};
use dfy_transport::{send_video_frame, CtrlChannel, Heartbeat};
use quinn::Connection;
use tokio::time::MissedTickBehavior;
use tracing::{debug, info};

use crate::capture_loop::CaptureLoop;

/// Agent 运行参数。
#[derive(Clone, Debug)]
pub struct AgentConfig {
    /// 最多发送多少帧后正常结束（`None` = 不限）。
    pub max_frames: Option<u64>,
    /// 无任何收发活动超过该时长即结束会话。
    pub idle_timeout: Duration,
    /// 正常收尾（帧数到上限）后等待在途 DATAGRAM 排空的时长，避免最后一帧被 close 丢弃。
    pub drain_delay: Duration,
    /// 会话结束后是否保留采集线程存活（断线重连/路径迁移时不重建采集与编码器）。
    ///
    /// 为 `true` 时 [`serve_agent`] 不会 `stop`/`join` 采集线程，调用方可以在下一个
    /// 连接里复用同一个 [`CaptureLoop`]；此时帧序号会从 0 重新开始，但编码器状态与
    /// 采集资源保持连续，重连后只需请求一次关键帧即可继续出图。
    pub stay_alive: bool,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_frames: None,
            idle_timeout: Duration::from_secs(3600),
            drain_delay: Duration::from_millis(50),
            stay_alive: false,
        }
    }
}

/// 调用方可以拦截 control 消息（如 input-event 走可靠流）与 input-move DATAGRAM。
pub trait CtrlHandler: Send {
    /// 处理一条非心跳的 control 消息；返回要回给对端的帧（通常 `None`）。
    fn on_ctrl(&mut self, _env: &Envelope) -> Result<Option<Envelope>> {
        Ok(None)
    }
    /// 处理一个非视频 DATAGRAM（当前只有 input-move）；`payload` 已去掉通道号首字节。
    fn on_datagram(&mut self, _chan: u8, _payload: &[u8]) -> Result<()> {
        Ok(())
    }
    /// 取出处理器需要额外发回对端的帧（一次 on_ctrl 可能产生多条，如剪贴板分块）。
    fn drain_outbox(&mut self) -> Vec<Envelope> {
        Vec::new()
    }
    /// 周期性回调（跟随 100ms ticker），用于本地状态轮询（如剪贴板序列号）。
    fn on_tick(&mut self) -> Result<()> {
        Ok(())
    }
}

/// 什么都不做的处理器（selftest / 只串流的场景）。
pub struct IgnoreHandler;

impl CtrlHandler for IgnoreHandler {}

/// Agent 侧一次会话的统计。
#[derive(Clone, Debug, Default)]
pub struct AgentRunStats {
    /// 发出的视频帧数。
    pub frames_sent: u64,
    /// 发出的视频 DATAGRAM 包数。
    pub packets_sent: u64,
    /// 发出的视频负载字节。
    pub bytes_sent: u64,
    /// 关键帧数。
    pub keyframes_sent: u64,
    /// 收到的 input-move DATAGRAM 数。
    pub datagrams_recv: u64,
    /// 最近一次 RTT（毫秒）。
    pub rtt_ms: Option<f64>,
    /// 结束原因（便于日志/测试断言）。
    pub end_reason: &'static str,
}

/// 跑一次 Agent 会话直到对端关闭 / 达到帧数上限 / 超时 / 采集结束。
pub async fn serve_agent(
    conn: Connection,
    welcome: Welcome,
    chan: CtrlChannel,
    capture: &mut CaptureLoop,
    mut handler: impl CtrlHandler,
    cfg: AgentConfig,
) -> Result<AgentRunStats> {
    let (mut tx, mut rx) = chan.split();
    let mut heartbeat = Heartbeat::new();
    let mut stats = AgentRunStats::default();
    let mut frames_left = cfg.max_frames;
    let mut frame_id: u32 = 0;
    let mut frame_seq: u16 = 0;
    let mut last_activity = Instant::now();
    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    info!(
        agent = %welcome.agent_name,
        session = %hex::encode(&welcome.session_id),
        frames = ?frames_left,
        "Agent 会话开始"
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
                if let Err(err) = handler.on_tick() {
                    debug!(error = %err, "处理器 tick 失败");
                }
                for extra in handler.drain_outbox() {
                    if let Err(err) = tx.send_ctrl(&extra).await {
                        debug!(error = %err, "发送处理器 tick 附加帧失败");
                    }
                }
                if last_activity.elapsed() > cfg.idle_timeout {
                    stats.end_reason = "idle_timeout";
                    break;
                }
                if frames_left == Some(0) {
                    stats.end_reason = "max_frames";
                    break;
                }
                if capture.is_stopping() {
                    stats.end_reason = "capture_stopping";
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
                        match env.body {
                            Some(envelope::Body::Ping(ping)) => {
                                if let Err(err) = tx.send_ctrl(&Envelope::pong(ping.ts_us)).await {
                                    debug!(error = %err, "回复 Pong 失败");
                                }
                            }
                            Some(envelope::Body::Pong(_)) => {
                                heartbeat.on_frame(&env);
                            }
                            Some(envelope::Body::Bye(bye)) => {
                                stats.end_reason = if bye.reason == dfy_proto::BYE_NORMAL {
                                    "peer_bye"
                                } else {
                                    "peer_bye_user"
                                };
                                break;
                            }
                            Some(envelope::Body::Reject(_)) => {
                                stats.end_reason = "rejected";
                                break;
                            }
                            _ => {
                                if let Ok(Some(reply)) = handler.on_ctrl(&env) {
                                    if let Err(err) = tx.send_ctrl(&reply).await {
                                        debug!(error = %err, "发送控制回复失败");
                                    }
                                }
                                for extra in handler.drain_outbox() {
                                    if let Err(err) = tx.send_ctrl(&extra).await {
                                        debug!(error = %err, "发送处理器附加帧失败");
                                    }
                                }
                            }
                        }
                    }
                }
            }

            frame = capture.frames.recv() => {
                match frame {
                    None => {
                        stats.end_reason = "capture_ended";
                        break;
                    }
                    Some(frame) => {
                        last_activity = Instant::now();
                        let ts = u32::try_from(frame.pts_us & u64::from(u32::MAX)).unwrap_or(0);
                        match send_video_frame(&conn, frame_id, frame_seq, ts, &frame.data, frame.keyframe) {
                            Ok(sent) => {
                                stats.frames_sent += 1;
                                stats.packets_sent += sent.packets as u64;
                                stats.bytes_sent += sent.bytes as u64;
                                if frame.keyframe {
                                    stats.keyframes_sent += 1;
                                }
                                frame_id = frame_id.wrapping_add(1);
                                frame_seq = frame_seq.wrapping_add(1);
                                frames_left = frames_left.map(|n| n.saturating_sub(1));
                                if frames_left == Some(0) {
                                    stats.end_reason = "max_frames";
                                    break;
                                }
                            }
                            Err(err) => {
                                debug!(error = %err, "发送视频 DATAGRAM 失败");
                                stats.end_reason = "send_failed";
                                break;
                            }
                        }
                    }
                }
            }

            datagram = conn.read_datagram() => {
                match datagram {
                    Ok(bytes) => {
                        if bytes.first() == Some(&CHAN_INPUT_MOVE) {
                            last_activity = Instant::now();
                            stats.datagrams_recv += 1;
                            if let Err(err) = handler.on_datagram(CHAN_INPUT_MOVE, &bytes[1..]) {
                                debug!(error = %err, "处理 input-move 失败");
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
    // 帧数/期望帧收满属于正常收尾：给在途 DATAGRAM 一点排空时间，避免最后一帧随 close 丢弃。
    if matches!(stats.end_reason, "max_frames" | "expect_frames") && !cfg.drain_delay.is_zero() {
        tokio::time::sleep(cfg.drain_delay).await;
    }
    // 尽力而为地发一个 Bye（对端可能已关闭）
    let _ = tx.send_ctrl(&Envelope::bye(dfy_proto::BYE_NORMAL)).await;
    tx.finish().ok();
    conn.close(0u32.into(), b"done");
    // 必须先请求采集线程退出，再 join：采集器可能在「无新帧」状态无限循环。
    if !cfg.stay_alive {
        capture.stop();
        if let Some(result) = capture.join() {
            match result {
                Ok(capture_stats) => debug!(
                    captured = capture_stats.captured,
                    encoded = capture_stats.encoded,
                    yielded = capture_stats.frames_yielded,
                    "采集线程已结束"
                ),
                Err(err) => debug!(error = %err, "采集线程异常结束"),
            }
        }
    } else {
        debug!("会话结束但保留采集线程（stay_alive），等待下一次连接复用");
    }
    info!(
        frames = stats.frames_sent,
        bytes = stats.bytes_sent,
        rtt_ms = ?stats.rtt_ms,
        reason = stats.end_reason,
        "Agent 会话结束"
    );
    Ok(stats)
}

/// 便于测试/工具代码：从 `Welcome` 取会话能力是否包含某名字。
pub fn has_capability(welcome: &Welcome, name: &str) -> bool {
    welcome.capability.iter().any(|c| c == name)
}

/// 便于调用方构造「视频配置」通知帧（可选，MVP 未强制）。
pub fn video_config(width: u32, height: u32, fps: u32, bitrate_kbps: u32) -> Envelope {
    Envelope::video_config(width, height, fps, bitrate_kbps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_config_defaults() {
        let cfg = AgentConfig::default();
        assert_eq!(cfg.max_frames, None);
        assert!(cfg.idle_timeout >= Duration::from_secs(60));
    }
}
