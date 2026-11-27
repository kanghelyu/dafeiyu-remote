// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 确定性的视频质量自适应控制器（纯逻辑、无 IO、无系统时钟）。
//!
//! 控制器每收到一个 [`PathSample`] 输出一份 [`AdaptationRecommendation`]。劣化与恢复
//! 使用不同阈值、连续采样数和冷却时间：短暂毛刺不会改变质量，持续拥塞逐步降低
//! 分辨率/帧率/码率，持续健康时再逐步恢复。变码率实际吞吐只用于限制码率，不单独
//! 代表链路容量；所有输出都受配置边界约束。

use crate::sample::{PathSample, MAX_LOSS_PPM};

/// 自适应配置或初始推荐非法时返回的错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdaptationError {
    message: String,
}

impl AdaptationError {
    fn new(message: String) -> Self {
        Self { message }
    }
}

impl std::fmt::Display for AdaptationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AdaptationError {}

macro_rules! ensure {
    ($condition:expr, $($arg:tt)*) => {
        if !$condition {
            return Err(AdaptationError::new(format!($($arg)*)));
        }
    };
}

/// 自适应分辨率；宽高均为非零偶数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdaptationResolution {
    /// 编码宽度（像素）。
    pub width: u32,
    /// 编码高度（像素）。
    pub height: u32,
}

impl AdaptationResolution {
    fn validate(self) -> std::result::Result<(), AdaptationError> {
        ensure!(
            self.width >= 2 && self.height >= 2 && self.width % 2 == 0 && self.height % 2 == 0,
            "dfy-path: 自适应分辨率必须为非零偶数（{}x{}）",
            self.width,
            self.height
        );
        Ok(())
    }
}

/// 自适应边界、劣化/恢复阈值和抗抖动参数。
#[derive(Debug, Clone, PartialEq)]
pub struct AdaptationConfig {
    /// 最小分辨率包围盒宽度。
    pub min_width: u32,
    /// 最小分辨率包围盒高度。
    pub min_height: u32,
    /// 最大分辨率包围盒宽度。
    pub max_width: u32,
    /// 最大分辨率包围盒高度。
    pub max_height: u32,
    /// 最低帧率（1..=120）。
    pub min_fps: u32,
    /// 最高帧率（1..=120）。
    pub max_fps: u32,
    /// 最低码率（kbps）。
    pub min_bitrate_kbps: u32,
    /// 最高码率（kbps）。
    pub max_bitrate_kbps: u32,
    /// 劣化 RTT 阈值（毫秒，严格大于）。
    pub congestion_rtt_ms: f32,
    /// 劣化丢包阈值（ppm，严格大于）。
    pub congestion_loss_ppm: u32,
    /// 劣化 stall 阈值（毫秒，严格大于）。
    pub congestion_stall_ms: u64,
    /// 恢复 RTT 阈值（毫秒，小于等于）。
    pub recovery_rtt_ms: f32,
    /// 恢复丢包上限（ppm，小于等于）。
    pub recovery_loss_ppm: u32,
    /// 恢复 stall 上限（毫秒，小于等于）。
    pub recovery_stall_ms: u64,
    /// 触发降档所需连续坏采样数。
    pub congestion_samples: u32,
    /// 触发升档所需连续好采样数。
    pub recovery_samples: u32,
    /// 任意两次质量变化的最小间隔（毫秒）。
    pub min_change_interval_ms: u64,
    /// 降档后升档的额外冷却时间（毫秒）。
    pub recovery_cooldown_ms: u64,
    /// 每次降档的分辨率/帧率倍率（0<factor<1）。
    pub downscale_factor: f32,
    /// 每次升档的分辨率/帧率倍率（factor>1）。
    pub upscale_factor: f32,
    /// 目标码率不得超过接收吞吐的该比例（0<factor<=1）。
    pub bitrate_throughput_fraction: f32,
}

impl Default for AdaptationConfig {
    fn default() -> Self {
        Self {
            min_width: 640,
            min_height: 360,
            max_width: 1920,
            max_height: 1080,
            min_fps: 15,
            max_fps: 120,
            min_bitrate_kbps: 500,
            max_bitrate_kbps: 20_000,
            congestion_rtt_ms: 150.0,
            congestion_loss_ppm: 30_000,
            congestion_stall_ms: 1_000,
            recovery_rtt_ms: 80.0,
            recovery_loss_ppm: 5_000,
            recovery_stall_ms: 500,
            congestion_samples: 2,
            recovery_samples: 5,
            min_change_interval_ms: 2_000,
            recovery_cooldown_ms: 10_000,
            downscale_factor: 0.8,
            upscale_factor: 1.25,
            bitrate_throughput_fraction: 0.8,
        }
    }
}

impl AdaptationConfig {
    /// 校验配置； [`AdaptationController::new`] 也会执行同一校验。
    pub fn validate(&self) -> std::result::Result<(), AdaptationError> {
        for (name, value) in [
            ("min_width", self.min_width),
            ("min_height", self.min_height),
            ("max_width", self.max_width),
            ("max_height", self.max_height),
        ] {
            ensure!(
                value >= 2 && value % 2 == 0,
                "dfy-path: {name} 必须为至少 2 的偶数（收到 {value}）"
            );
        }
        ensure!(
            self.min_width <= self.max_width && self.min_height <= self.max_height,
            "dfy-path: 最小分辨率包围盒不能大于最大包围盒"
        );
        ensure!(
            (1..=120).contains(&self.min_fps) && self.min_fps <= self.max_fps,
            "dfy-path: min_fps 必须在 1..=120 且不大于 max_fps"
        );
        ensure!(
            (1..=120).contains(&self.max_fps),
            "dfy-path: max_fps 必须在 1..=120（收到 {}）",
            self.max_fps
        );
        ensure!(
            self.min_bitrate_kbps > 0 && self.min_bitrate_kbps <= self.max_bitrate_kbps,
            "dfy-path: 码率边界必须为正且 min <= max"
        );
        for (name, value) in [
            ("congestion_rtt_ms", self.congestion_rtt_ms),
            ("recovery_rtt_ms", self.recovery_rtt_ms),
            ("downscale_factor", self.downscale_factor),
            ("upscale_factor", self.upscale_factor),
            (
                "bitrate_throughput_fraction",
                self.bitrate_throughput_fraction,
            ),
        ] {
            ensure!(
                value.is_finite() && value > 0.0,
                "dfy-path: {name} 必须为有限正数"
            );
        }
        ensure!(
            self.downscale_factor < 1.0
                && self.upscale_factor > 1.0
                && self.bitrate_throughput_fraction <= 1.0,
            "dfy-path: 吞吐/升降档倍率范围非法"
        );
        ensure!(
            self.congestion_samples > 0 && self.recovery_samples > 0,
            "dfy-path: 连续采样数必须为正"
        );
        ensure!(
            self.congestion_loss_ppm <= MAX_LOSS_PPM && self.recovery_loss_ppm <= MAX_LOSS_PPM,
            "dfy-path: 丢包阈值不能超过 1_000_000 ppm"
        );
        ensure!(
            self.congestion_rtt_ms > self.recovery_rtt_ms
                && self.congestion_loss_ppm > self.recovery_loss_ppm
                && self.congestion_stall_ms > self.recovery_stall_ms,
            "dfy-path: 恢复阈值必须比劣化阈值更严格，以形成迟滞"
        );
        Ok(())
    }
}

/// 自适应调整方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdaptationDirection {
    /// 保持当前质量。
    Hold,
    /// 降低质量。
    Down,
    /// 提高质量。
    Up,
}

/// 本次没有改变质量的原因，或本次调整的依据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdaptationReason {
    /// 控制器刚建立，尚未发生质量变化。
    Initial,
    /// 坏采样尚未达到连续次数要求。
    WaitingForCongestion,
    /// 好采样尚未达到连续次数要求。
    WaitingForRecovery,
    /// 处于变化冷却期。
    Cooldown,
    /// 已到配置下界。
    AtMinimum,
    /// 已到配置上界。
    AtMaximum,
    /// 持续坏采样触发降档。
    Congested,
    /// 持续好采样且有吞吐余量，触发升档。
    Recovered,
    /// 时间戳非单调，忽略本次采样。
    StaleSample,
}

/// 一次质量推荐。`changed == false` 时其余字段与当前生效质量相同。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdaptationRecommendation {
    /// 推荐编码分辨率。
    pub resolution: AdaptationResolution,
    /// 推荐帧率（fps）。
    pub fps: u32,
    /// 推荐目标码率（kbps）。
    pub bitrate_kbps: u32,
    /// 本次采样是否导致质量变化。
    pub changed: bool,
    /// 调整方向。
    pub direction: AdaptationDirection,
    /// 调整/保持原因。
    pub reason: AdaptationReason,
}

/// 确定性视频质量自适应控制器。
#[derive(Debug, Clone)]
pub struct AdaptationController {
    config: AdaptationConfig,
    current: AdaptationRecommendation,
    native_resolution: AdaptationResolution,
    congestion_streak: u32,
    recovery_streak: u32,
    last_change_at_ms: Option<u64>,
    last_direction: AdaptationDirection,
    last_sample_at_ms: Option<u64>,
}

impl AdaptationController {
    /// 创建控制器并检查边界。初始推荐会被限制在配置和采集分辨率内。
    pub fn new(
        config: AdaptationConfig,
        initial_resolution: AdaptationResolution,
        initial_fps: u32,
        initial_bitrate_kbps: u32,
    ) -> std::result::Result<Self, AdaptationError> {
        Self::new_with_ceiling(
            config,
            initial_resolution,
            initial_resolution,
            initial_fps,
            initial_bitrate_kbps,
        )
    }

    /// 从当前已降级质量创建控制器，并指定恢复时可达的独立分辨率上限。
    pub fn new_with_ceiling(
        config: AdaptationConfig,
        initial_resolution: AdaptationResolution,
        ceiling_resolution: AdaptationResolution,
        initial_fps: u32,
        initial_bitrate_kbps: u32,
    ) -> std::result::Result<Self, AdaptationError> {
        config.validate()?;
        initial_resolution.validate()?;
        ceiling_resolution.validate()?;
        ensure!(
            (1..=120).contains(&initial_fps),
            "dfy-path: 初始 fps 必须在 1..=120"
        );
        let native_resolution = ceiling_resolution;
        let resolution = bounded_resolution(initial_resolution, native_resolution, &config)?;
        let current = AdaptationRecommendation {
            resolution,
            fps: initial_fps.clamp(config.min_fps, config.max_fps),
            bitrate_kbps: initial_bitrate_kbps
                .clamp(config.min_bitrate_kbps, config.max_bitrate_kbps),
            changed: false,
            direction: AdaptationDirection::Hold,
            reason: AdaptationReason::Initial,
        };
        Ok(Self {
            config,
            current,
            native_resolution,
            congestion_streak: 0,
            recovery_streak: 0,
            last_change_at_ms: None,
            last_direction: AdaptationDirection::Hold,
            last_sample_at_ms: None,
        })
    }

    /// 生效配置。
    pub fn config(&self) -> &AdaptationConfig {
        &self.config
    }

    /// 当前推荐质量。
    pub fn current(&self) -> AdaptationRecommendation {
        self.current
    }

    /// 消费一个网络/播放采样，返回确定性的质量推荐。
    ///
    /// RTT 为 NaN 表示本次没有 RTT 观测，不会单独触发 RTT 劣化；吞吐、丢包和 stall
    /// 仍参与判断。时间戳必须单调递增，重复或倒退的采样被忽略。
    pub fn on_sample(&mut self, sample: PathSample) -> AdaptationRecommendation {
        if self
            .last_sample_at_ms
            .is_some_and(|last| sample.at_ms <= last)
        {
            return self.hold(AdaptationReason::StaleSample);
        }
        self.last_sample_at_ms = Some(sample.at_ms);
        let sample = sample.clamped();

        if self.is_congested(sample) {
            self.recovery_streak = 0;
            self.congestion_streak = self.congestion_streak.saturating_add(1);
            if self.congestion_streak < self.config.congestion_samples {
                return self.hold(AdaptationReason::WaitingForCongestion);
            }
            if !self.change_interval_elapsed(sample.at_ms, false) {
                return self.hold(AdaptationReason::Cooldown);
            }
            return self.degrade(sample);
        }

        self.congestion_streak = 0;
        if !self.is_recovered(sample) {
            self.recovery_streak = 0;
            return self.hold(AdaptationReason::WaitingForRecovery);
        }
        self.recovery_streak = self.recovery_streak.saturating_add(1);
        if self.recovery_streak < self.config.recovery_samples {
            return self.hold(AdaptationReason::WaitingForRecovery);
        }
        if !self.change_interval_elapsed(sample.at_ms, true) {
            return self.hold(AdaptationReason::Cooldown);
        }
        self.recover(sample)
    }

    fn is_congested(&self, sample: PathSample) -> bool {
        // VBR goodput below the target does not imply a constrained path.
        let rtt_bad = sample.has_rtt() && sample.rtt_ms > self.config.congestion_rtt_ms;
        rtt_bad
            || sample.loss_ppm > self.config.congestion_loss_ppm
            || sample.stall_ms > self.config.congestion_stall_ms
    }

    fn is_recovered(&self, sample: PathSample) -> bool {
        let rtt_good = !sample.has_rtt() || sample.rtt_ms <= self.config.recovery_rtt_ms;
        rtt_good
            && sample.loss_ppm <= self.config.recovery_loss_ppm
            && sample.stall_ms <= self.config.recovery_stall_ms
    }

    fn change_interval_elapsed(&self, at_ms: u64, recovery: bool) -> bool {
        let Some(last) = self.last_change_at_ms else {
            return true;
        };
        let cooldown = self.config.min_change_interval_ms.saturating_add(
            if recovery && self.last_direction == AdaptationDirection::Down {
                self.config.recovery_cooldown_ms
            } else {
                0
            },
        );
        at_ms.saturating_sub(last) >= cooldown
    }

    fn degrade(&mut self, sample: PathSample) -> AdaptationRecommendation {
        let factor = self.config.downscale_factor;
        let resolution = scale_resolution(
            self.current.resolution,
            factor,
            self.native_resolution,
            &self.config,
        );
        let fps = scale_u32(
            self.current.fps,
            factor,
            self.config.min_fps,
            self.config.max_fps,
        );
        let throughput_kbps = if sample.recv_bps == 0 {
            self.config.min_bitrate_kbps
        } else {
            ((sample.recv_bps as f64 * f64::from(self.config.bitrate_throughput_fraction) / 1000.0)
                .round() as u64)
                .min(u64::from(u32::MAX)) as u32
        };
        let bitrate = scale_u32(
            self.current.bitrate_kbps,
            factor,
            self.config.min_bitrate_kbps,
            self.config.max_bitrate_kbps,
        )
        .min(throughput_kbps)
        .max(self.config.min_bitrate_kbps);
        if resolution == self.current.resolution
            && fps == self.current.fps
            && bitrate == self.current.bitrate_kbps
        {
            return self.hold(AdaptationReason::AtMinimum);
        }
        self.commit(
            resolution,
            fps,
            bitrate,
            sample.at_ms,
            AdaptationDirection::Down,
            AdaptationReason::Congested,
        )
    }

    fn recover(&mut self, sample: PathSample) -> AdaptationRecommendation {
        let factor = self.config.upscale_factor;
        let resolution = scale_resolution(
            self.current.resolution,
            factor,
            self.native_resolution,
            &self.config,
        );
        let fps = scale_u32(
            self.current.fps,
            factor,
            self.config.min_fps,
            self.config.max_fps,
        );
        if resolution == self.current.resolution
            && fps == self.current.fps
            && self.current.bitrate_kbps >= self.config.max_bitrate_kbps
        {
            return self.hold(AdaptationReason::AtMaximum);
        }
        let throughput_kbps =
            ((sample.recv_bps as f64 * f64::from(self.config.bitrate_throughput_fraction) / 1000.0)
                .round() as u64)
                .min(u64::from(u32::MAX)) as u32;
        let bitrate = scale_u32(
            self.current.bitrate_kbps,
            factor,
            self.config.min_bitrate_kbps,
            self.config.max_bitrate_kbps,
        )
        .min(throughput_kbps)
        .min(self.config.max_bitrate_kbps)
        .max(self.config.min_bitrate_kbps);
        if resolution == self.current.resolution
            && fps == self.current.fps
            && bitrate == self.current.bitrate_kbps
        {
            return self.hold(AdaptationReason::AtMaximum);
        }
        self.commit(
            resolution,
            fps,
            bitrate,
            sample.at_ms,
            AdaptationDirection::Up,
            AdaptationReason::Recovered,
        )
    }

    fn hold(&mut self, reason: AdaptationReason) -> AdaptationRecommendation {
        self.current.changed = false;
        self.current.direction = AdaptationDirection::Hold;
        self.current.reason = reason;
        self.current
    }

    #[allow(clippy::too_many_arguments)]
    fn commit(
        &mut self,
        resolution: AdaptationResolution,
        fps: u32,
        bitrate_kbps: u32,
        at_ms: u64,
        direction: AdaptationDirection,
        reason: AdaptationReason,
    ) -> AdaptationRecommendation {
        self.current = AdaptationRecommendation {
            resolution,
            fps,
            bitrate_kbps,
            changed: true,
            direction,
            reason,
        };
        self.congestion_streak = 0;
        self.recovery_streak = 0;
        self.last_change_at_ms = Some(at_ms);
        self.last_direction = direction;
        self.current
    }
}

fn scale_u32(value: u32, factor: f32, min: u32, max: u32) -> u32 {
    ((f64::from(value) * f64::from(factor)).round() as u64).clamp(u64::from(min), u64::from(max))
        as u32
}

fn scale_resolution(
    current: AdaptationResolution,
    factor: f32,
    native: AdaptationResolution,
    config: &AdaptationConfig,
) -> AdaptationResolution {
    let width = ((f64::from(current.width) * f64::from(factor)).round() as u64) & !1;
    // 由采集面的原始宽高比推导另一轴，避免连续缩放积累比例漂移。
    let height = ((width * u64::from(native.height)) / u64::from(native.width)) & !1;
    let scaled = AdaptationResolution {
        width: width.max(2) as u32,
        height: height.max(2) as u32,
    };
    // 边界来自同一宽高比，fit 不会返回错误。
    bounded_resolution(scaled, native, config).unwrap_or(current)
}

fn bounded_resolution(
    resolution: AdaptationResolution,
    native: AdaptationResolution,
    config: &AdaptationConfig,
) -> std::result::Result<AdaptationResolution, AdaptationError> {
    let max = AdaptationResolution {
        width: config.max_width.min(native.width),
        height: config.max_height.min(native.height),
    };
    let min = AdaptationResolution {
        width: config.min_width,
        height: config.min_height,
    };
    let max_fit = fit_aspect(max, native)?;
    let min_fit = fit_aspect(min, native)?;
    ensure!(
        min_fit.width <= max_fit.width && min_fit.height <= max_fit.height,
        "dfy-path: 最小/最大分辨率包围盒与采集宽高比不兼容"
    );
    let fitted = fit_aspect(
        AdaptationResolution {
            width: resolution.width.min(max_fit.width),
            height: resolution.height.min(max_fit.height),
        },
        native,
    )?;
    if fitted.width < min_fit.width || fitted.height < min_fit.height {
        Ok(min_fit)
    } else {
        Ok(fitted)
    }
}

fn fit_aspect(
    bounds: AdaptationResolution,
    aspect: AdaptationResolution,
) -> std::result::Result<AdaptationResolution, AdaptationError> {
    let aw = u64::from(aspect.width);
    let ah = u64::from(aspect.height);
    let (width, height) = if u64::from(bounds.width) * ah <= u64::from(bounds.height) * aw {
        (
            u64::from(bounds.width),
            round_div(u64::from(bounds.width) * ah, aw),
        )
    } else {
        (
            round_div(u64::from(bounds.height) * aw, ah),
            u64::from(bounds.height),
        )
    };
    let result = AdaptationResolution {
        width: (width.max(2) as u32) & !1,
        height: (height.max(2) as u32) & !1,
    };
    result.validate()?;
    Ok(result)
}

fn round_div(numerator: u64, denominator: u64) -> u64 {
    let quotient = numerator / denominator;
    let remainder = numerator % denominator;
    quotient + u64::from(remainder >= denominator - remainder)
}
