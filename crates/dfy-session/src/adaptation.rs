// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

use std::time::Instant;

use anyhow::{Context, Result};
use dfy_path::{
    AdaptationConfig, AdaptationController, AdaptationRecommendation, AdaptationResolution,
    PathSample,
};
use dfy_proto::StreamFeedback;
use quinn::ConnectionStats;
use tokio::sync::Mutex;

use crate::capture_loop::{CaptureRuntimeControl, EncodeResolution, RuntimeSettings};

#[derive(Debug, Clone, Default)]
pub struct StreamAdaptationConfig {
    pub tuning: AdaptationConfig,
}

#[derive(Debug, Default)]
pub(crate) struct AdaptationState {
    host_limit: Option<RuntimeSettings>,
    user_limit: Option<RuntimeSettings>,
    controller: Option<AdaptationController>,
    applied: Option<RuntimeSettings>,
    last_applied: Option<RuntimeSettings>,
    last_at_ms: u64,
    session_started_at: Option<Instant>,
}

impl AdaptationState {
    pub(crate) fn new(host_limit: RuntimeSettings) -> Self {
        Self {
            host_limit: Some(host_limit),
            ..Self::default()
        }
    }

    pub(crate) fn note_applied(&mut self, settings: RuntimeSettings) {
        self.applied = Some(settings);
        self.last_applied = Some(settings);
    }

    fn begin_session(&mut self, started_at: Instant, at_ms: u64) -> bool {
        if self.session_started_at == Some(started_at) {
            return false;
        }
        self.session_started_at = Some(started_at);
        self.controller = None;
        self.last_at_ms = at_ms;
        true
    }

    fn ensure_controller(
        &mut self,
        config: &StreamAdaptationConfig,
        current: RuntimeSettings,
        native: EncodeResolution,
        limit: RuntimeSettings,
    ) -> Result<()> {
        if self.controller.is_some() {
            return Ok(());
        }
        let settings =
            if let Some(previous) = self.last_applied.filter(|previous| *previous != current) {
                let settings = controller_for_previous(config, previous, current, native, limit)?;
                tracing::debug!(
                    width = settings.current().resolution.width,
                    height = settings.current().resolution.height,
                    fps = settings.current().fps,
                    bitrate_kbps = settings.current().bitrate_kbps,
                    "恢复跨会话自适应状态"
                );
                settings
            } else {
                new_controller(config, current, native, limit)?
            };
        self.controller = Some(settings);
        self.applied = Some(current);
        Ok(())
    }

    fn effective_limit(&self) -> Result<RuntimeSettings> {
        let host = self
            .host_limit
            .context("dfy-session: 自适应启动上限未初始化")?;
        match self.user_limit {
            Some(user) => cap_settings(host, user),
            None => Ok(host),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct NetworkStats {
    pub(crate) sent_packets: u64,
    pub(crate) lost_packets: u64,
}

/// 平台确认值越界且无法恢复到一个可确认的安全设置。
///
/// 这种错误不能像普通“本次设置未生效”一样继续会话，否则编码器可能停留在越界状态。
#[derive(Debug, Clone, Copy)]
pub(crate) struct FatalSettingsConfirmation;

impl std::fmt::Display for FatalSettingsConfirmation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("dfy-session: 平台确认画质越界且回滚失败")
    }
}

impl std::error::Error for FatalSettingsConfirmation {}

pub(crate) fn is_fatal_settings_confirmation(error: &anyhow::Error) -> bool {
    error.downcast_ref::<FatalSettingsConfirmation>().is_some()
}

pub(crate) fn feedback_is_valid(feedback: &StreamFeedback) -> bool {
    (1..=60_000).contains(&feedback.window_ms)
        && (feedback.packets > 0 || feedback.dropped_frames > 0)
        && (feedback.packets > 0 || feedback.payload_bytes == 0)
        && feedback.frames_delivered <= feedback.frames_assembled
        && feedback.stall_ms <= 60_000
}

pub(crate) async fn apply_user_limit(
    runtime: &CaptureRuntimeControl,
    state: &Mutex<AdaptationState>,
    user_limit: Option<RuntimeSettings>,
) -> Result<RuntimeSettings> {
    let mut guard = state.lock().await;
    let current = runtime.current();
    // 只有在尚未应用过任何用户/自适应设置时，当前值才可能是启动时的原生确认值。
    // 不能从已经被用户上限或自适应降级污染的 current 反推 source，否则 Reset 后
    // 会把恢复天花板永久压低。
    if guard.user_limit.is_none() && guard.applied.is_none() {
        if runtime.source_resolution().is_none() {
            runtime.publish_source_resolution(current.resolution)?;
        }
        if let Some(host) = guard.host_limit.as_mut() {
            if host.resolution.is_none() {
                host.resolution = current.resolution;
            }
        }
    }
    let host = guard
        .host_limit
        .context("dfy-session: 自适应启动上限未初始化")?;
    let requested = user_limit.map_or(Ok(host), |user| cap_settings(host, user))?;
    requested.validate()?;
    let fallback = guard.last_applied.unwrap_or(current);
    let applied = apply_confirmed_settings(runtime, requested, fallback).await?;
    // 启动时 resolution=None 表示“使用原生采集面”。Reset 到该状态后，平台返回的
    // 具体分辨率可补全缺失的 source/host；已有 source 时只允许补 host，不覆盖 source。
    if user_limit.is_none()
        && guard
            .host_limit
            .as_ref()
            .and_then(|host| host.resolution)
            .is_none()
    {
        let source = runtime.source_resolution();
        let native = source.or(applied.resolution);
        if let Some(native) = native {
            // 已知 source 是原生上限，不能被平台返回的具体尺寸覆盖。
            if source.is_none() {
                runtime.publish_source_resolution(Some(native))?;
            }
            if let Some(host) = guard.host_limit.as_mut() {
                host.resolution = Some(native);
            }
        }
    }
    guard.user_limit = user_limit;
    guard.controller = None;
    guard.note_applied(applied);
    Ok(applied)
}

/// 应用设置并核对平台确认值；越界时恢复旧设置，并再次核对恢复结果。
///
/// 普通越界（恢复成功）返回错误但允许会话继续。恢复失败或恢复值仍越界时返回
/// [`FatalSettingsConfirmation`]，调用方必须结束会话。
async fn apply_confirmed_settings(
    runtime: &CaptureRuntimeControl,
    requested: RuntimeSettings,
    fallback: RuntimeSettings,
) -> Result<RuntimeSettings> {
    requested.validate()?;
    fallback.validate()?;
    let source = runtime.source_resolution();
    let runtime_for_apply = runtime.clone();
    let applied = tokio::task::spawn_blocking(move || runtime_for_apply.apply(requested)).await??;
    let actual = runtime.current();
    if actual == applied && confirmed_within_limit_and_source(&applied, &requested, source) {
        return Ok(applied);
    }

    let runtime_for_rollback = runtime.clone();
    let restored = match tokio::task::spawn_blocking(move || runtime_for_rollback.apply(fallback))
        .await
    {
        Ok(Ok(restored)) => restored,
        Ok(Err(error)) => {
            let actual = runtime.current();
            return Err(FatalSettingsConfirmation).with_context(|| {
                format!(
                    "dfy-session: 平台确认越界后恢复失败（请求 {requested:?}，确认 {applied:?}，当前 {actual:?}）：{error:#}"
                )
            });
        }
        Err(error) => {
            let actual = runtime.current();
            return Err(FatalSettingsConfirmation).with_context(|| {
                format!(
                    "dfy-session: 平台确认越界后恢复任务失败（请求 {requested:?}，确认 {applied:?}，当前 {actual:?}）：{error}"
                )
            });
        }
    };
    let actual = runtime.current();
    if actual != restored || !confirmed_within_limit_and_source(&restored, &fallback, source) {
        return Err(FatalSettingsConfirmation).with_context(|| {
            format!(
                "dfy-session: 平台确认越界后恢复值仍无效（请求 {requested:?}，确认 {applied:?}，恢复 {restored:?}，当前 {actual:?}）"
            )
        });
    }
    anyhow::bail!(
        "dfy-session: 平台确认的画质超出请求上限，已恢复旧设置（请求 {requested:?}，确认 {applied:?}，恢复 {restored:?}）"
    )
}

pub(crate) async fn apply_feedback(
    runtime: &CaptureRuntimeControl,
    state: &Mutex<AdaptationState>,
    config: &StreamAdaptationConfig,
    feedback: &StreamFeedback,
    stats: ConnectionStats,
    previous: &mut NetworkStats,
    started_at: Instant,
) -> Result<Option<AdaptationRecommendation>> {
    let (loss_ppm, recv_bps) = feedback_metrics(feedback, stats, previous);
    *previous = NetworkStats {
        sent_packets: stats.path.sent_packets,
        lost_packets: stats.path.lost_packets,
    };
    let at_ms = started_at.elapsed().as_millis().min(u64::MAX as u128) as u64;
    let mut sample = PathSample {
        rtt_ms: stats.path.rtt.as_millis().min(60_000) as f32,
        loss_ppm,
        recv_bps,
        stall_ms: feedback.stall_ms,
        at_ms,
    };
    tracing::debug!(
        rtt_ms = sample.rtt_ms,
        loss_ppm,
        recv_bps,
        stall_ms = feedback.stall_ms,
        packets = feedback.packets,
        frames_assembled = feedback.frames_assembled,
        frames_delivered = feedback.frames_delivered,
        dropped_frames = feedback.dropped_frames,
        "自适应采样"
    );

    let mut guard = state.lock().await;
    let current = runtime.current();
    let Some(current_resolution) = current.resolution else {
        return Ok(None);
    };
    let limit = guard.effective_limit()?;
    let native = runtime
        .source_resolution()
        .or(limit.resolution)
        .unwrap_or(current_resolution);
    guard.begin_session(started_at, at_ms);
    let at_ms = guard.last_at_ms.saturating_add(1).max(at_ms);
    guard.last_at_ms = at_ms;
    sample.at_ms = at_ms;
    guard.ensure_controller(config, current, native, limit)?;
    let recommendation = guard
        .controller
        .as_mut()
        .expect("controller initialized")
        .on_sample(sample);
    if !recommendation.changed {
        return Ok(Some(recommendation));
    }
    let requested = RuntimeSettings {
        fps: recommendation.fps,
        bitrate_kbps: recommendation.bitrate_kbps,
        resolution: Some(EncodeResolution {
            width: recommendation.resolution.width,
            height: recommendation.resolution.height,
        }),
    };
    let fallback = guard.last_applied.unwrap_or(current);
    let applied = match apply_confirmed_settings(runtime, requested, fallback).await {
        Ok(applied) => applied,
        Err(error) => {
            let current = runtime.current();
            guard.applied = Some(current);
            if is_fatal_settings_confirmation(&error) {
                return Err(error);
            }
            guard.controller = Some(controller_for_previous(
                config,
                guard.last_applied.unwrap_or(current),
                current,
                native,
                limit,
            )?);
            return Err(error);
        }
    };
    let resolution = applied
        .resolution
        .context("dfy-session: 平台未返回已确认的编码分辨率")?;
    let confirmed = AdaptationRecommendation {
        resolution: AdaptationResolution {
            width: resolution.width,
            height: resolution.height,
        },
        fps: applied.fps,
        bitrate_kbps: applied.bitrate_kbps,
        changed: true,
        direction: recommendation.direction,
        reason: recommendation.reason,
    };
    guard.note_applied(applied);
    tracing::info!(
        width = resolution.width,
        height = resolution.height,
        fps = applied.fps,
        bitrate_kbps = applied.bitrate_kbps,
        direction = ?recommendation.direction,
        reason = ?recommendation.reason,
        "自适应画质已生效"
    );
    Ok(Some(confirmed))
}

fn feedback_metrics(
    feedback: &StreamFeedback,
    stats: ConnectionStats,
    previous: &NetworkStats,
) -> (u32, u64) {
    let sent = stats
        .path
        .sent_packets
        .saturating_sub(previous.sent_packets);
    let lost = stats
        .path
        .lost_packets
        .saturating_sub(previous.lost_packets);
    let network_loss = if sent == 0 && lost == 0 {
        0
    } else {
        (u64::from(u32::MAX).min(lost.saturating_mul(1_000_000) / sent.max(lost).max(1)))
            .min(1_000_000) as u32
    };
    let frames = feedback
        .frames_assembled
        .saturating_add(feedback.dropped_frames);
    let frame_loss = feedback
        .dropped_frames
        .saturating_mul(1_000_000)
        .checked_div(frames)
        .unwrap_or(0)
        .min(1_000_000) as u32;
    let recv_bps = if feedback.packets < 2 {
        u64::MAX
    } else {
        feedback
            .payload_bytes
            .saturating_mul(8)
            .saturating_mul(1000)
            .checked_div(feedback.window_ms)
            .unwrap_or(u64::MAX)
    };
    (network_loss.max(frame_loss), recv_bps)
}

fn cap_settings(host: RuntimeSettings, user: RuntimeSettings) -> Result<RuntimeSettings> {
    user.validate()?;
    let resolution = match (host.resolution, user.resolution) {
        (Some(source), Some(limit)) => Some(limit.fit(source.width, source.height)?),
        (None, user) => user,
        (host, None) => host,
    };
    Ok(RuntimeSettings {
        fps: user.fps.min(host.fps),
        bitrate_kbps: user.bitrate_kbps.min(host.bitrate_kbps),
        resolution,
    })
}

fn confirmed_within_limit(applied: &RuntimeSettings, requested: &RuntimeSettings) -> bool {
    if applied.fps != requested.fps || applied.bitrate_kbps != requested.bitrate_kbps {
        return false;
    }
    match (applied.resolution, requested.resolution) {
        // requested=None 表示使用原生采集面；平台返回具体原生尺寸或 None 都合法。
        (_, None) => true,
        (Some(applied), Some(limit)) => {
            applied.width <= limit.width && applied.height <= limit.height
        }
        (None, Some(_)) => false,
    }
}

fn confirmed_within_limit_and_source(
    applied: &RuntimeSettings,
    requested: &RuntimeSettings,
    source: Option<EncodeResolution>,
) -> bool {
    if !confirmed_within_limit(applied, requested) {
        return false;
    }
    match (applied.resolution, source) {
        (Some(applied), Some(source)) => {
            applied.width <= source.width && applied.height <= source.height
        }
        _ => true,
    }
}

fn new_controller(
    config: &StreamAdaptationConfig,
    current: RuntimeSettings,
    native: EncodeResolution,
    limit: RuntimeSettings,
) -> Result<AdaptationController> {
    new_controller_with_initial(config, current, native, limit)
}

fn controller_for_previous(
    config: &StreamAdaptationConfig,
    previous: RuntimeSettings,
    current: RuntimeSettings,
    native: EncodeResolution,
    limit: RuntimeSettings,
) -> Result<AdaptationController> {
    let previous = RuntimeSettings {
        resolution: previous
            .resolution
            .map(|resolution| resolution.fit(native.width, native.height))
            .transpose()?,
        fps: previous.fps.max(current.fps),
        bitrate_kbps: previous.bitrate_kbps.max(current.bitrate_kbps),
    };
    new_controller_with_initial(config, previous, native, limit)
}

fn new_controller_with_initial(
    config: &StreamAdaptationConfig,
    initial: RuntimeSettings,
    native: EncodeResolution,
    limit: RuntimeSettings,
) -> Result<AdaptationController> {
    let ceiling = limit
        .resolution
        .unwrap_or(native)
        .fit(native.width, native.height)?;
    let mut bounded = config.tuning.clone();
    bounded.min_width = (u64::from(ceiling.width) / 4).max(2) as u32 & !1;
    bounded.min_height = (u64::from(ceiling.height) / 4).max(2) as u32 & !1;
    bounded.max_width = ceiling.width;
    bounded.max_height = ceiling.height;
    bounded.min_fps = bounded.min_fps.min(limit.fps);
    bounded.max_fps = limit.fps;
    bounded.min_bitrate_kbps = bounded.min_bitrate_kbps.min(limit.bitrate_kbps);
    bounded.max_bitrate_kbps = limit.bitrate_kbps;
    let initial_resolution = initial
        .resolution
        .context("dfy-session: 自适应当前分辨率未确认")?;
    AdaptationController::new_with_ceiling(
        bounded,
        AdaptationResolution {
            width: initial_resolution.width,
            height: initial_resolution.height,
        },
        AdaptationResolution {
            width: ceiling.width,
            height: ceiling.height,
        },
        initial.fps,
        initial.bitrate_kbps,
    )
    .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::Duration,
    };

    use super::*;
    use crate::capture_loop::CaptureLoop;

    fn feedback() -> StreamFeedback {
        StreamFeedback {
            window_ms: 500,
            payload_bytes: 500_000,
            packets: 10,
            frames_assembled: 30,
            frames_delivered: 30,
            dropped_frames: 0,
            stall_ms: 0,
        }
    }

    fn settings(fps: u32, bitrate_kbps: u32, width: u32, height: u32) -> RuntimeSettings {
        RuntimeSettings {
            fps,
            bitrate_kbps,
            resolution: Some(EncodeResolution { width, height }),
        }
    }

    #[test]
    fn rejects_invalid_feedback_windows() {
        assert!(feedback_is_valid(&feedback()));
        let mut bad = feedback();
        bad.window_ms = 0;
        assert!(!feedback_is_valid(&bad));
        bad = feedback();
        bad.packets = 0;
        assert!(!feedback_is_valid(&bad));
        bad = feedback();
        bad.frames_delivered = 31;
        assert!(!feedback_is_valid(&bad));
    }

    #[test]
    fn computes_payload_throughput_and_frame_loss() {
        let mut value = feedback();
        value.dropped_frames = 10;
        let (loss, bps) =
            feedback_metrics(&value, ConnectionStats::default(), &NetworkStats::default());
        assert_eq!(loss, 250_000);
        assert_eq!(bps, 8_000_000);
    }

    #[test]
    fn uses_connection_loss_deltas() {
        let mut stats = ConnectionStats::default();
        stats.path.sent_packets = 200;
        stats.path.lost_packets = 20;
        let previous = NetworkStats {
            sent_packets: 100,
            lost_packets: 10,
        };
        assert_eq!(feedback_metrics(&feedback(), stats, &previous).0, 100_000);
    }

    #[test]
    fn user_limits_are_preserved() {
        let config = StreamAdaptationConfig::default();
        let current = settings(120, 12_345, 3840, 2160);
        let controller = new_controller(
            &config,
            current,
            EncodeResolution {
                width: 3840,
                height: 2160,
            },
            current,
        )
        .unwrap();
        assert_eq!(controller.current().fps, 120);
        assert_eq!(controller.current().bitrate_kbps, 12_345);
        assert_eq!(controller.config().max_width, 3840);
        assert_eq!(controller.config().max_bitrate_kbps, 12_345);
    }

    #[test]
    fn user_ceiling_does_not_ratchet_down_after_autonomous_degradation() {
        let config = StreamAdaptationConfig::default();
        let host = settings(120, 20_000, 1920, 1080);
        let degraded = settings(48, 4_800, 1024, 576);
        let controller = new_controller(&config, degraded, host.resolution.unwrap(), host).unwrap();
        assert_eq!(controller.current().resolution.width, 1024);
        assert_eq!(controller.config().max_width, 1920);
        assert_eq!(controller.config().max_fps, 120);
        assert_eq!(controller.config().max_bitrate_kbps, 20_000);
    }

    #[test]
    fn previous_degraded_quality_is_not_ratcheted_into_a_new_session_ceiling() {
        let config = StreamAdaptationConfig::default();
        let host = settings(120, 20_000, 1920, 1080);
        let previous = settings(48, 4_800, 1024, 576);
        let current = settings(50, 5_000, 1194, 752);
        let controller =
            controller_for_previous(&config, previous, current, host.resolution.unwrap(), host)
                .unwrap();
        assert_eq!(controller.current().fps, 50);
        assert_eq!(controller.current().resolution.width, 1024);
        assert_eq!(controller.current().resolution.height, 576);
        assert_eq!(controller.current().bitrate_kbps, 5_000);
        assert_eq!(controller.config().max_width, 1920);
        assert_eq!(controller.config().max_height, 1080);
        assert_eq!(controller.config().max_fps, 120);
        assert_eq!(controller.config().max_bitrate_kbps, 20_000);
    }

    #[test]
    fn previous_settings_preserve_recovery_progress() {
        let config = StreamAdaptationConfig::default();
        let host = settings(120, 20_000, 1920, 1080);
        let previous = settings(50, 5_000, 1194, 752);
        let current = settings(48, 4_800, 1024, 576);
        let controller =
            controller_for_previous(&config, previous, current, host.resolution.unwrap(), host)
                .unwrap();
        assert_eq!(controller.current().fps, 50);
        assert_eq!(controller.current().resolution.width, 1190);
        assert_eq!(controller.current().resolution.height, 670);
        assert_eq!(controller.current().bitrate_kbps, 5_000);
        assert_eq!(controller.config().max_width, 1920);
        assert_eq!(controller.config().max_fps, 120);
    }

    #[test]
    fn new_session_clock_rebuilds_controller_without_repeated_resets() {
        let config = StreamAdaptationConfig::default();
        let host = settings(120, 20_000, 1920, 1080);
        let current = settings(48, 4_800, 1024, 576);
        let previous_session = Instant::now();
        let current_session = previous_session + Duration::from_secs(60);
        let mut state = AdaptationState::new(host);
        state.note_applied(current);
        state.controller =
            Some(new_controller(&config, current, host.resolution.unwrap(), host).unwrap());
        state.session_started_at = Some(previous_session);
        state.last_at_ms = 30_000;
        assert!(state.begin_session(current_session, 500));
        assert!(state.controller.is_none());
        assert_eq!(state.last_at_ms, 500);
        state
            .ensure_controller(&config, current, host.resolution.unwrap(), host)
            .unwrap();
        let mut sample = PathSample {
            rtt_ms: 200.0,
            loss_ppm: 100_000,
            recv_bps: 8_000_000,
            stall_ms: 0,
            at_ms: 501,
        };
        let first = state.controller.as_mut().unwrap().on_sample(sample);
        assert_eq!(
            first.reason,
            dfy_path::AdaptationReason::WaitingForCongestion
        );
        assert!(!first.changed);
        assert!(!state.begin_session(current_session, 800));
        assert!(state.controller.is_some());
        sample.at_ms = 800;
        let second = state.controller.as_mut().unwrap().on_sample(sample);
        assert_eq!(second.reason, dfy_path::AdaptationReason::Congested);
        assert!(second.changed);
    }

    #[test]
    fn confirmed_settings_rebuild_a_missing_controller() {
        let config = StreamAdaptationConfig::default();
        let host = settings(120, 20_000, 1920, 1080);
        let mut state = AdaptationState::new(host);
        state.note_applied(host);
        state.controller = None;
        state
            .ensure_controller(&config, host, host.resolution.unwrap(), host)
            .unwrap();
        assert!(state.controller.is_some());
    }

    #[test]
    fn platform_native_resolution_is_accepted_for_an_unbounded_request() {
        let requested = RuntimeSettings::new(120, 12_000);
        let native = settings(120, 12_000, 1920, 1080);
        assert!(confirmed_within_limit(&native, &requested));
        assert!(confirmed_within_limit(&requested, &requested));
        assert!(!confirmed_within_limit(
            &RuntimeSettings::new(120, 12_000),
            &native
        ));
    }

    #[test]
    fn native_confirmation_cannot_exceed_or_overwrite_a_known_source() {
        let requested = RuntimeSettings::new(120, 12_000);
        let source = EncodeResolution {
            width: 1920,
            height: 1080,
        };
        let oversized = settings(120, 12_000, 3840, 2160);
        assert!(!confirmed_within_limit_and_source(
            &oversized,
            &requested,
            Some(source)
        ));
        assert!(confirmed_within_limit_and_source(
            &settings(120, 12_000, 1920, 1080),
            &requested,
            Some(source)
        ));

        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let current = settings(60, 4_000, 640, 360);
            let calls = Arc::new(AtomicUsize::new(0));
            let apply_calls = Arc::clone(&calls);
            let (capture, _input) = CaptureLoop::external_with_runtime_and_source(
                1,
                current,
                Some(source),
                move |_| {
                    let call = apply_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(if call == 0 { oversized } else { current })
                },
            )
            .unwrap();
            let control = capture.runtime_control();
            let mut adaptation = AdaptationState::new(requested);
            adaptation.note_applied(current);
            let state = Arc::new(Mutex::new(adaptation));

            let error = apply_user_limit(&control, &state, None).await.unwrap_err();
            assert!(!is_fatal_settings_confirmation(&error));
            assert!(error.to_string().contains("已恢复旧设置"));
            assert_eq!(control.current(), current);
            assert_eq!(control.source_resolution(), Some(source));
            assert_eq!(state.lock().await.host_limit.unwrap().resolution, None);
            assert_eq!(calls.load(Ordering::SeqCst), 2);
            capture.stop();
        });
    }

    #[test]
    fn reset_to_native_preserves_known_source_and_fills_missing_host_limit() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let source = EncodeResolution {
                width: 1920,
                height: 1080,
            };
            let current = settings(60, 4_000, 640, 360);
            let host = RuntimeSettings::new(120, 12_000);
            let (capture, _input) = CaptureLoop::external_with_runtime_and_source(
                1,
                current,
                Some(source),
                move |requested| {
                    Ok(RuntimeSettings {
                        resolution: Some(source),
                        ..requested
                    })
                },
            )
            .unwrap();
            let control = capture.runtime_control();
            let mut adaptation = AdaptationState::new(host);
            adaptation.note_applied(current);
            let state = Arc::new(Mutex::new(adaptation));

            let applied = apply_user_limit(&control, &state, None).await.unwrap();
            assert_eq!(applied.resolution, Some(source));
            assert_eq!(control.source_resolution(), Some(source));
            assert_eq!(
                state.lock().await.host_limit.unwrap().resolution,
                Some(source)
            );
            capture.stop();
        });
    }

    #[test]
    fn source_violating_rollback_is_fatal() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let initial = settings(120, 12_000, 1920, 1080);
            let source = initial.resolution.unwrap();
            let invalid = settings(60, 4_000, 800, 600);
            let oversized_restore = settings(120, 12_000, 3840, 2160);
            let calls = Arc::new(AtomicUsize::new(0));
            let apply_calls = Arc::clone(&calls);
            let (capture, _input) = CaptureLoop::external_with_runtime_and_source(
                1,
                initial,
                Some(source),
                move |_| {
                    let call = apply_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(if call == 0 {
                        invalid
                    } else {
                        oversized_restore
                    })
                },
            )
            .unwrap();
            let control = capture.runtime_control();
            let state = Arc::new(Mutex::new(AdaptationState::new(initial)));
            state.lock().await.note_applied(initial);

            let error = apply_user_limit(&control, &state, Some(settings(60, 4_000, 640, 360)))
                .await
                .unwrap_err();
            assert!(is_fatal_settings_confirmation(&error));
            assert_eq!(control.current(), oversized_restore);
            assert_eq!(control.source_resolution(), Some(source));
            assert_eq!(calls.load(Ordering::SeqCst), 2);
            capture.stop();
        });
    }

    #[test]
    fn platform_aspect_fitting_is_accepted_within_the_user_ceiling() {
        let requested = settings(30, 500, 316, 200);
        let applied = settings(30, 500, 316, 198);
        assert!(confirmed_within_limit(&applied, &requested));
        assert!(!confirmed_within_limit(
            &settings(30, 500, 318, 198),
            &requested
        ));
        assert!(!confirmed_within_limit(
            &settings(30, 500, 316, 202),
            &requested
        ));
        assert!(!confirmed_within_limit(
            &settings(31, 500, 316, 198),
            &requested
        ));
        assert!(!confirmed_within_limit(
            &settings(30, 501, 316, 198),
            &requested
        ));
    }

    #[test]
    fn user_resolution_is_fitted_to_host_aspect_without_upscaling() {
        let host = settings(120, 20_000, 1920, 1080);
        let user = settings(60, 8_000, 3840, 2160);
        let capped = cap_settings(host, user).unwrap();
        assert_eq!(capped.fps, 60);
        assert_eq!(capped.bitrate_kbps, 8_000);
        assert_eq!(
            capped.resolution,
            Some(EncodeResolution {
                width: 1920,
                height: 1080
            })
        );
    }

    #[test]
    fn missing_source_is_not_inferred_from_user_limit_and_reset_restores_native_ceiling() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let initial = RuntimeSettings::new(120, 12_000);
            let native = EncodeResolution {
                width: 1920,
                height: 1080,
            };
            let (capture, _input) =
                CaptureLoop::external_with_runtime_and_source(1, initial, None, move |requested| {
                    Ok(RuntimeSettings {
                        resolution: requested.resolution.or(Some(native)),
                        ..requested
                    })
                })
                .unwrap();
            let control = capture.runtime_control();
            let state = Arc::new(Mutex::new(AdaptationState::new(initial)));
            let user = settings(60, 4_000, 640, 360);

            let applied = apply_user_limit(&control, &state, Some(user))
                .await
                .unwrap();
            assert_eq!(applied.resolution, user.resolution);
            assert_eq!(control.source_resolution(), None);
            assert_eq!(state.lock().await.host_limit.unwrap().resolution, None);

            let reset = apply_user_limit(&control, &state, None).await.unwrap();
            assert_eq!(reset.resolution, Some(native));
            assert_eq!(control.source_resolution(), Some(native));
            let host = state.lock().await.host_limit.unwrap();
            assert_eq!(host.resolution, Some(native));

            let mut guard = state.lock().await;
            guard.controller = None;
            guard
                .ensure_controller(&StreamAdaptationConfig::default(), reset, native, host)
                .unwrap();
            let controller = guard.controller.as_ref().unwrap();
            assert_eq!(controller.config().max_width, native.width);
            assert_eq!(controller.config().max_height, native.height);
            assert_eq!(controller.config().max_fps, 120);
            assert_eq!(controller.config().max_bitrate_kbps, 12_000);
            drop(guard);
            capture.stop();
        });
    }

    #[test]
    fn out_of_range_user_confirmation_rolls_back_before_returning_error() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let initial = settings(120, 12_000, 1920, 1080);
            let invalid = settings(60, 4_000, 800, 600);
            let calls = Arc::new(AtomicUsize::new(0));
            let apply_calls = Arc::clone(&calls);
            let (capture, _input) = CaptureLoop::external_with_runtime_and_source(
                1,
                initial,
                initial.resolution,
                move |_| {
                    let call = apply_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(if call == 0 { invalid } else { initial })
                },
            )
            .unwrap();
            let control = capture.runtime_control();
            let state = Arc::new(Mutex::new(AdaptationState::new(initial)));
            state.lock().await.note_applied(initial);

            let error = apply_user_limit(&control, &state, Some(settings(60, 4_000, 640, 360)))
                .await
                .unwrap_err();
            assert!(!is_fatal_settings_confirmation(&error));
            assert!(error.to_string().contains("已恢复旧设置"));
            assert_eq!(calls.load(Ordering::SeqCst), 2);
            assert_eq!(control.current(), initial);
            let guard = state.lock().await;
            assert_eq!(guard.user_limit, None);
            assert_eq!(guard.last_applied, Some(initial));
            assert_eq!(guard.applied, Some(initial));
            drop(guard);
            capture.stop();
        });
    }

    #[test]
    fn failed_rollback_is_fatal_and_reports_the_actual_platform_settings() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let initial = settings(120, 12_000, 1920, 1080);
            let invalid = settings(60, 4_000, 800, 600);
            let calls = Arc::new(AtomicUsize::new(0));
            let apply_calls = Arc::clone(&calls);
            let (capture, _input) = CaptureLoop::external_with_runtime_and_source(
                1,
                initial,
                initial.resolution,
                move |_| {
                    let call = apply_calls.fetch_add(1, Ordering::SeqCst);
                    if call == 0 {
                        Ok(invalid)
                    } else {
                        anyhow::bail!("injected rollback failure")
                    }
                },
            )
            .unwrap();
            let control = capture.runtime_control();
            let state = Arc::new(Mutex::new(AdaptationState::new(initial)));
            state.lock().await.note_applied(initial);

            let error = apply_user_limit(&control, &state, Some(settings(60, 4_000, 640, 360)))
                .await
                .unwrap_err();
            assert!(is_fatal_settings_confirmation(&error));
            let detail = format!("{error:#}");
            assert!(detail.contains("width: 800"));
            assert!(detail.contains("height: 600"));
            assert!(detail.contains("injected rollback failure"));
            assert_eq!(calls.load(Ordering::SeqCst), 2);
            assert_eq!(control.current(), invalid);
            capture.stop();
        });
    }

    #[test]
    fn invalid_rollback_confirmation_is_fatal() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let initial = settings(120, 12_000, 1920, 1080);
            let invalid = settings(60, 4_000, 800, 600);
            let invalid_restore = settings(60, 4_000, 900, 700);
            let calls = Arc::new(AtomicUsize::new(0));
            let apply_calls = Arc::clone(&calls);
            let (capture, _input) = CaptureLoop::external_with_runtime_and_source(
                1,
                initial,
                initial.resolution,
                move |_| {
                    let call = apply_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(if call == 0 { invalid } else { invalid_restore })
                },
            )
            .unwrap();
            let control = capture.runtime_control();
            let state = Arc::new(Mutex::new(AdaptationState::new(initial)));
            state.lock().await.note_applied(initial);

            let error = apply_user_limit(&control, &state, Some(settings(60, 4_000, 640, 360)))
                .await
                .unwrap_err();
            assert!(is_fatal_settings_confirmation(&error));
            assert!(error.to_string().contains("恢复值仍无效"));
            assert_eq!(calls.load(Ordering::SeqCst), 2);
            assert_eq!(control.current(), invalid_restore);
            capture.stop();
        });
    }
}
