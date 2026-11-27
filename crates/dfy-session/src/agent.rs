// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! Agent（被控端）会话：把采集线程产出的码流持续发成视频 DATAGRAM，同时处理 control 流
//! （心跳、Bye、自定义消息）与客户端发来的 input-move DATAGRAM。

use std::{
    sync::{atomic::Ordering, Arc},
    time::{Duration, Instant},
};

use anyhow::{ensure, Result};
use dfy_proto::{
    envelope, Envelope, QualityControl, Welcome, CAPABILITY_QUALITY_CONTROL, CHAN_INPUT_MOVE,
    PERM_CONTROL,
};
use dfy_transport::{send_video_frame, CtrlChannel, Heartbeat};
use quinn::Connection;
use tokio::{
    sync::{mpsc::Receiver, Mutex as AsyncMutex},
    time::MissedTickBehavior,
};
use tracing::{debug, info};

use crate::{
    adaptation::{
        apply_feedback, apply_user_limit, feedback_is_valid, is_fatal_settings_confirmation,
        NetworkStats, StreamAdaptationConfig,
    },
    capture_loop::{
        CaptureActivityGuard, CaptureControlRefs, CaptureHandle, CaptureLoop, EncodeResolution,
        RuntimeSettings,
    },
    migration::{MigrationAckFailed, MigrationContext, SessionCoordinator, SessionLease},
    EncodedFrame,
};

enum FrameSource<'a> {
    Direct(&'a mut Receiver<EncodedFrame>),
    Shared(Arc<AsyncMutex<Receiver<EncodedFrame>>>),
}

impl FrameSource<'_> {
    async fn recv(&mut self) -> Option<EncodedFrame> {
        match self {
            Self::Direct(rx) => rx.recv().await,
            Self::Shared(rx) => rx.lock().await.recv().await,
        }
    }
}

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
    /// Signed cross-connection migration and shared stream ownership.
    pub migration: Option<MigrationContext>,
    /// Coordinator shared by every connection handled by the same agent.
    pub coordinator: SessionCoordinator,
    /// Receiver-driven quality adaptation; `None` keeps the user-selected settings fixed.
    pub adaptation: Option<StreamAdaptationConfig>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_frames: None,
            idle_timeout: Duration::from_secs(3600),
            drain_delay: Duration::from_millis(50),
            stay_alive: false,
            migration: None,
            coordinator: SessionCoordinator::default(),
            adaptation: None,
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
    /// 收到的 SessionResume（重连/路径迁移）次数。
    pub resumes: u64,
    /// Accepted receiver quality windows.
    pub feedback_windows: u64,
    /// Quality changes applied by the adaptive controller.
    pub adaptation_changes: u64,
    /// Received viewer quality-control requests.
    pub quality_controls: u64,
    /// Rejected, throttled, or failed viewer quality-control requests.
    pub quality_control_errors: u64,
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
    handler: impl CtrlHandler,
    cfg: AgentConfig,
) -> Result<AgentRunStats> {
    let stay_alive = cfg.stay_alive;
    capture.pause_when_idle.store(stay_alive, Ordering::SeqCst);
    while capture.frames.try_recv().is_ok() {}
    let control = CaptureControlRefs {
        stop: &capture.stop_flag,
        keyframe: &capture.keyframe_flag,
        runtime: &capture.runtime,
        adaptation: &capture.adaptation,
        active_sessions: &capture.active_sessions,
        activity: &capture.activity,
    };
    let mut frames = FrameSource::Direct(&mut capture.frames);
    let result = serve_agent_inner(conn, welcome, chan, &control, &mut frames, handler, cfg).await;
    if !stay_alive {
        capture.stop();
        if let Some(Err(err)) = capture.join() {
            debug!(error = %err, "采集线程异常结束");
        }
    }
    result
}

/// 并发连接版本：迁移中的新旧连接共享采集队列，只有当前代次会消费视频帧。
pub async fn serve_agent_shared(
    conn: Connection,
    welcome: Welcome,
    chan: CtrlChannel,
    capture: &CaptureHandle,
    handler: impl CtrlHandler,
    cfg: AgentConfig,
) -> Result<AgentRunStats> {
    capture
        .pause_when_idle
        .store(cfg.stay_alive, Ordering::SeqCst);
    let fresh_session = cfg
        .migration
        .as_ref()
        .map_or(true, |context| context.hello.resume_session_id.is_empty());
    if cfg.stay_alive && fresh_session && capture.active_sessions.load(Ordering::SeqCst) == 0 {
        let mut pending = capture.frames.lock().await;
        while pending.try_recv().is_ok() {}
    }
    let control = capture.control_refs();
    let mut frames = FrameSource::Shared(Arc::clone(&capture.frames));
    serve_agent_inner(conn, welcome, chan, &control, &mut frames, handler, cfg).await
}

async fn serve_agent_inner(
    conn: Connection,
    welcome: Welcome,
    chan: CtrlChannel,
    capture: &CaptureControlRefs<'_>,
    frames: &mut FrameSource<'_>,
    mut handler: impl CtrlHandler,
    cfg: AgentConfig,
) -> Result<AgentRunStats> {
    let (mut tx, mut rx) = chan.split();
    let mut lease = match &cfg.migration {
        Some(context) => Some(SessionLease::for_welcome(context, &welcome)?),
        None => None,
    };
    let mut heartbeat = Heartbeat::new();
    let mut stats = AgentRunStats::default();
    let mut frames_left = cfg.max_frames;
    let mut frame_id: u32 = 0;
    let mut frame_seq: u16 = 0;
    let mut last_activity = Instant::now();
    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let started_at = Instant::now();
    let mut previous_network = NetworkStats {
        sent_packets: conn.stats().path.sent_packets,
        lost_packets: conn.stats().path.lost_packets,
    };
    let mut last_feedback_at: Option<Instant> = None;
    let mut last_keyframe_request: Option<Instant> = None;
    let mut last_quality_request: Option<Instant> = None;
    let mut initial_config_sent = false;

    info!(
        agent = %welcome.agent_name,
        session = %hex::encode(&welcome.session_id),
        frames = ?frames_left,
        "Agent 会话开始"
    );
    let pending_migration = lease.as_ref().is_some_and(SessionLease::is_pending);
    // stop 检查与 activity 激活必须在同一把 transition 锁内完成，避免并发会话在
    // 另一会话设置致命 stop 后短暂重新激活编码器。
    let acquired = if pending_migration {
        CaptureActivityGuard::acquire_pending_if_running(capture.activity)
    } else {
        CaptureActivityGuard::acquire_if_running(capture.active_sessions, capture.activity)
    };
    let mut activity = match acquired {
        Some(activity) => Some(activity),
        None => {
            stats.end_reason = "capture_stopping";
            return Ok(stats);
        }
    };
    if !pending_migration {
        capture.keyframe.store(true, Ordering::SeqCst);
        last_keyframe_request = Some(Instant::now());
    }

    loop {
        tokio::select! {
            biased;

            _ = async {
                match &lease {
                    Some(lease) => lease.changed().await,
                    None => std::future::pending().await,
                }
            }, if cfg.migration.is_some() => {
                if lease.as_ref().is_some_and(SessionLease::was_superseded) {
                    stats.end_reason = "migrated";
                    break;
                }
            }

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
                if capture.stop.load(Ordering::SeqCst) {
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
                            Some(envelope::Body::StreamFeedback(feedback)) => {
                                let active = lease.as_ref().map_or(true, |lease| {
                                    !lease.is_pending() && !lease.was_superseded()
                                });
                                let accepted = active
                                    && feedback_is_valid(&feedback)
                                    && last_feedback_at.map_or(true, |last| {
                                        last.elapsed() >= Duration::from_millis(200)
                                    });
                                if accepted {
                                    last_feedback_at = Some(Instant::now());
                                    stats.feedback_windows += 1;
                                    if let Some(config) = &cfg.adaptation {
                                        match apply_feedback(
                                            capture.runtime,
                                            capture.adaptation,
                                            config,
                                            &feedback,
                                            conn.stats(),
                                            &mut previous_network,
                                            started_at,
                                        )
                                        .await
                                        {
                                            Ok(Some(recommendation)) => {
                                                if recommendation.changed {
                                                    stats.adaptation_changes += 1;
                                                    let notice = Envelope::video_config(
                                                        recommendation.resolution.width,
                                                        recommendation.resolution.height,
                                                        recommendation.fps,
                                                        recommendation.bitrate_kbps,
                                                    );
                                                    if let Err(err) = tx.send_ctrl(&notice).await {
                                                        debug!(error = %err, "发送自适应视频配置失败");
                                                    }
                                                    initial_config_sent = true;
                                                    info!(
                                                        fps = recommendation.fps,
                                                        bitrate_kbps = recommendation.bitrate_kbps,
                                                        width = recommendation.resolution.width,
                                                        height = recommendation.resolution.height,
                                                        reason = ?recommendation.reason,
                                                        "已应用自适应视频质量"
                                                    );
                                                }
                                            }
                                            Ok(None) => {}
                                            Err(err) => {
                                                if is_fatal_settings_confirmation(&err) {
                                                    capture.activity.stop(capture.stop);
                                                    stats.end_reason = "quality_confirmation_failed";
                                                    debug!(error = %err, "自适应画质确认越界且回滚失败，结束会话");
                                                    break;
                                                }
                                                debug!(error = %err, "应用自适应视频质量失败");
                                            }
                                        }
                                    }
                                }
                            }
                            Some(envelope::Body::QualityControl(request)) => {
                                stats.quality_controls += 1;
                                let active = lease.as_ref().map_or(true, |lease| {
                                    !lease.is_pending() && !lease.was_superseded()
                                });
                                let allowed = match &lease {
                                    Some(lease) => lease.quality_control_allowed(),
                                    None => {
                                        welcome.permission_bitmap & PERM_CONTROL != 0
                                            && has_capability(&welcome, CAPABILITY_QUALITY_CONTROL)
                                    }
                                };
                                let throttled = last_quality_request.is_some_and(|last| {
                                    last.elapsed() < Duration::from_secs(1)
                                });
                                if !active || !allowed || throttled {
                                    stats.quality_control_errors += 1;
                                    debug!(active, allowed, throttled, "忽略画质控制请求");
                                    continue;
                                }
                                last_quality_request = Some(Instant::now());
                                match quality_limit(&request) {
                                    Ok(limit) => {
                                        match apply_user_limit(
                                            capture.runtime,
                                            capture.adaptation,
                                            limit,
                                        )
                                        .await
                                        {
                                            Ok(applied) => {
                                                let resolution = applied.resolution.ok_or_else(|| {
                                                    anyhow::anyhow!("平台未确认编码分辨率")
                                                });
                                                match resolution {
                                                    Ok(resolution) => {
                                                        capture.keyframe.store(true, Ordering::SeqCst);
                                                        if let Err(err) = tx
                                                            .send_ctrl(&Envelope::video_config(
                                                                resolution.width,
                                                                resolution.height,
                                                                applied.fps,
                                                                applied.bitrate_kbps,
                                                            ))
                                                            .await
                                                        {
                                                            debug!(error = %err, "发送画质确认失败");
                                                        }
                                                        initial_config_sent = true;
                                                        info!(
                                                            width = resolution.width,
                                                            height = resolution.height,
                                                            fps = applied.fps,
                                                            bitrate_kbps = applied.bitrate_kbps,
                                                            "用户画质上限已生效"
                                                        );
                                                    }
                                                    Err(err) => {
                                                        stats.quality_control_errors += 1;
                                                        debug!(error = %err, "画质设置缺少分辨率确认");
                                                    }
                                                }
                                            }
                                            Err(err) => {
                                                stats.quality_control_errors += 1;
                                                if is_fatal_settings_confirmation(&err) {
                                                    capture.activity.stop(capture.stop);
                                                    stats.end_reason = "quality_confirmation_failed";
                                                    debug!(error = %err, "用户画质确认越界且回滚失败，结束会话");
                                                    break;
                                                }
                                                debug!(error = %err, "应用用户画质上限失败");
                                            }
                                        }
                                    }
                                    Err(err) => {
                                        stats.quality_control_errors += 1;
                                        debug!(error = %err, "拒绝无效画质控制请求");
                                    }
                                }
                            }
                            Some(envelope::Body::KeyframeRequest(_)) => {
                                let active = lease.as_ref().map_or(true, |lease| {
                                    !lease.is_pending() && !lease.was_superseded()
                                });
                                if active
                                    && last_keyframe_request.map_or(true, |last| {
                                        last.elapsed() >= Duration::from_secs(1)
                                    })
                                {
                                    capture.keyframe.store(true, Ordering::SeqCst);
                                    last_keyframe_request = Some(Instant::now());
                                }
                            }
                            Some(envelope::Body::Reject(_)) => {
                                stats.end_reason = "rejected";
                                break;
                            }
                            Some(envelope::Body::SessionResume(resume)) => {
                                stats.resumes += 1;
                                if let Some(context) = &cfg.migration {
                                    let migrated = lease.as_mut().expect("migration lease exists");
                                    let rollback_activation = resume.phase
                                        == dfy_proto::SESSION_RESUME_COMMIT
                                        && !activity
                                            .as_ref()
                                            .expect("capture activity guard exists")
                                            .is_active();
                                    let commit = || {
                                        let activated = activity
                                            .as_mut()
                                            .expect("capture activity guard exists")
                                            .activate();
                                        anyhow::ensure!(activated, "capture stopped before migration commit");
                                        Ok(())
                                    };
                                    if let Err(err) = migrated
                                        .handle_resume_with_commit(context, &mut tx, &resume, commit)
                                        .await
                                    {
                                        if err.downcast_ref::<MigrationAckFailed>().is_some() {
                                            debug!(error = %err, "迁移已提交但确认发送失败，保留状态等待幂等重试");
                                            continue;
                                        }
                                        if rollback_activation {
                                            activity
                                                .as_mut()
                                                .expect("capture activity guard exists")
                                                .deactivate();
                                        }
                                        if capture.stop.load(Ordering::SeqCst) {
                                            stats.end_reason = "capture_stopping";
                                            debug!(error = %err, "采集已停止，迁移 COMMIT 未确认");
                                            break;
                                        }
                                        let _ = tx
                                            .send_ctrl(&Envelope::reject(dfy_proto::ERR_PROTOCOL))
                                            .await;
                                        stats.end_reason = "migration_rejected";
                                        debug!(error = %err, "会话迁移被拒绝");
                                        break;
                                    }
                                    if resume.phase == dfy_proto::SESSION_RESUME_COMMIT {
                                        capture.keyframe.store(true, Ordering::SeqCst);
                                        info!(
                                            session = %hex::encode(&resume.session_id),
                                            "会话迁移已提交，新连接接管发帧"
                                        );
                                    }
                                } else {
                                    stats.end_reason = "migration_unsupported";
                                    let _ = tx
                                        .send_ctrl(&Envelope::reject(dfy_proto::ERR_PROTOCOL))
                                        .await;
                                    break;
                                }
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

            frame = frames.recv(), if lease.as_ref().map_or(true, |lease| !lease.is_pending()) => {
                match frame {
                    None => {
                        stats.end_reason = "capture_ended";
                        break;
                    }
                    Some(frame) => {
                        last_activity = Instant::now();
                        if !initial_config_sent {
                            let current = capture.runtime.current();
                            if let Some(resolution) = current.resolution {
                                if let Err(err) = tx
                                    .send_ctrl(&Envelope::video_config(
                                        resolution.width,
                                        resolution.height,
                                        current.fps,
                                        current.bitrate_kbps,
                                    ))
                                    .await
                                {
                                    debug!(error = %err, "发送初始视频配置失败");
                                }
                                initial_config_sent = true;
                            }
                        }
                        let ts = u32::try_from(frame.pts_us & u64::from(u32::MAX)).unwrap_or(0);
                        let sent = if let Some(migrated) = &lease {
                            match migrated.send_frame(&conn, ts, &frame.data, frame.keyframe) {
                                Ok(sent) => sent,
                                Err(err) => {
                                    debug!(error = %err, "发送视频 DATAGRAM 失败");
                                    stats.end_reason = "send_failed";
                                    break;
                                }
                            }
                        } else {
                            match send_video_frame(&conn, frame_id, frame_seq, ts, &frame.data, frame.keyframe) {
                                Ok(sent) => {
                                    frame_id = frame_id.wrapping_add(1);
                                    frame_seq = frame_seq.wrapping_add(1);
                                    Some(sent)
                                }
                                Err(err) => {
                                    debug!(error = %err, "发送视频 DATAGRAM 失败");
                                    stats.end_reason = "send_failed";
                                    break;
                                }
                            }
                        };
                        let Some(sent) = sent else {
                            stats.end_reason = "migrated";
                            break;
                        };
                        stats.frames_sent += 1;
                        stats.packets_sent += sent.packets as u64;
                        stats.bytes_sent += sent.bytes as u64;
                        if frame.keyframe {
                            stats.keyframes_sent += 1;
                        }
                        frames_left = frames_left.map(|n| n.saturating_sub(1));
                        if frames_left == Some(0) {
                            stats.end_reason = "max_frames";
                            break;
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

    if let (Some(context), Some(migrated)) = (&cfg.migration, &mut lease) {
        migrated.finish(
            context,
            matches!(stats.end_reason, "peer_bye" | "peer_bye_user"),
        );
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
    if cfg.stay_alive {
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

fn quality_limit(request: &QualityControl) -> Result<Option<RuntimeSettings>> {
    let all_zero =
        request.width == 0 && request.height == 0 && request.fps == 0 && request.bitrate_kbps == 0;
    if all_zero {
        return Ok(None);
    }
    ensure!(
        request.width > 0 && request.height > 0 && request.fps > 0 && request.bitrate_kbps > 0,
        "dfy-session: 非重置画质请求的四个字段必须全部非零"
    );
    let settings = RuntimeSettings {
        fps: request.fps,
        bitrate_kbps: request.bitrate_kbps,
        resolution: Some(EncodeResolution {
            width: request.width,
            height: request.height,
        }),
    };
    settings.validate()?;
    Ok(Some(settings))
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
    fn quality_requests_require_complete_fields_and_valid_ranges() {
        assert_eq!(
            quality_limit(&QualityControl {
                width: 0,
                height: 0,
                fps: 0,
                bitrate_kbps: 0,
            })
            .unwrap(),
            None
        );
        assert!(quality_limit(&QualityControl {
            width: 1920,
            height: 1080,
            fps: 0,
            bitrate_kbps: 8000,
        })
        .is_err());
        assert!(quality_limit(&QualityControl {
            width: 1919,
            height: 1080,
            fps: 120,
            bitrate_kbps: 8000,
        })
        .is_err());
        assert!(quality_limit(&QualityControl {
            width: 1920,
            height: 1080,
            fps: 121,
            bitrate_kbps: 8000,
        })
        .is_err());
        let valid = quality_limit(&QualityControl {
            width: 1920,
            height: 1080,
            fps: 120,
            bitrate_kbps: 100_000,
        })
        .unwrap()
        .unwrap();
        assert_eq!(valid.fps, 120);
        assert_eq!(valid.bitrate_kbps, 100_000);
    }

    #[test]
    fn agent_config_defaults() {
        let cfg = AgentConfig::default();
        assert_eq!(cfg.max_frames, None);
        assert!(cfg.idle_timeout >= Duration::from_secs(60));
    }
}
