// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 切换决策的可配置阈值。
//!
//! 默认值针对「局域网/公网 P2P 直连 + 自建中继」这一目标场景，调参理由见
//! `design/028-path-migration.md` §5。三条原则：
//! - **劣化判定要「持续」而非瞬时**：3 个坏采样、间隔 ≥500ms（约 1.0~1.5s 才动）；
//! - **切回 P2P 要「更严格、更慢」**：5 个好采样 + 距上次切换 ≥30s（抗抖动）；
//! - **观察期只抓「立即劣化」**：切换后 5s 内 1.5s 连续 3 个坏采样就回滚，
//!   与常规劣化判定区分，且中继路径不参与 RTT 倍率判定（中继天然 RTT 更高）。
//!
//! 所有阈值都允许调用方覆盖；[`PathConfig::sanitized`] 会把明显的非法值（0 容量、
//! NaN 系数等）修正为默认值，因此 [`crate::PathMonitor::new`] 永远不会 panic。

use crate::sample::MAX_LOSS_PPM;

/// 路径质量监测与切换的阈值配置。
#[derive(Debug, Clone, PartialEq)]
pub struct PathConfig {
    /// 滑动窗口容量（采样条数），默认 32。
    pub window_capacity: usize,
    /// 平滑 RTT 的 EWMA 系数 α，默认 0.2（越大越灵敏）。
    pub ewma_alpha: f32,
    /// 建立 RTT 基线所需的采样条数，默认 5。
    pub baseline_samples: usize,
    /// 参与基线采样的最大 `stall_ms`，默认 500ms（卡顿时建立的基线不可信）。
    pub baseline_max_stall_ms: u64,
    /// 基线的 RTT 下限，默认 5ms（回环/同机场景基线可能 <1ms，导致 2× 阈值过严）。
    pub baseline_rtt_floor_ms: f32,
    /// P2P → relay 需要的连续坏采样数，默认 3。
    pub degrade_bad_samples: u32,
    /// 坏采样之间的最小间隔，默认 500ms（防止 3 个瞬时毛刺触发切换）。
    pub degrade_min_spacing_ms: u64,
    /// RTT 劣化倍率：RTT > `baseline × factor` 视为坏采样，默认 2.0。
    pub degrade_rtt_factor: f32,
    /// 丢包劣化阈值（ppm），默认 20_000（2%）。
    pub degrade_loss_ppm: u32,
    /// 视频 stall 劣化阈值（毫秒），默认 2_000（2s）。
    pub degrade_stall_ms: u64,
    /// relay → P2P 需要的连续好采样数，默认 5。
    pub recover_good_samples: u32,
    /// 恢复判定的 RTT 倍率：RTT ≤ `baseline × factor`，默认 1.2。
    pub recover_rtt_factor: f32,
    /// 恢复判定的丢包上限（ppm），默认 5_000（0.5%）。
    pub recover_loss_ppm: u32,
    /// 没有基线（例如首连即走中继）时的恢复 RTT 绝对阈值（毫秒），默认 120。
    pub recover_abs_rtt_ms: f32,
    /// 距上一次切换的最小间隔（冷期），默认 30_000ms；只约束 relay → P2P。
    pub min_switch_interval_ms: u64,
    /// 切换后的观察期时长，默认 5_000ms。
    pub observe_window_ms: u64,
    /// 观察期内触发回滚的坏采样数，默认 3。
    pub rollback_bad_samples: u32,
    /// 观察期内触发回滚的坏采样时间跨度上限，默认 1_500ms。
    pub rollback_span_ms: u64,
    /// 回滚/切换失败后的退避时长，默认 5_000ms（期间不切换）。
    pub backoff_ms: u64,
    /// 中继路径上请求 P2P 探测的最小间隔，默认 1_000ms。
    pub probe_interval_ms: u64,
    /// 探测采样的新鲜度上限，默认 2_000ms（超过则重新 `RequestProbe`）。
    pub probe_stale_ms: u64,
    /// 切换请求的等待上限，默认 5_000ms；超时视为切换失败并退避。
    ///
    /// 与 `dfy-session` 的 `SessionResume` 等待上限（≤5s）对齐。
    pub switch_timeout_ms: u64,
}

impl Default for PathConfig {
    fn default() -> Self {
        Self {
            window_capacity: 32,
            ewma_alpha: 0.2,
            baseline_samples: 5,
            baseline_max_stall_ms: 500,
            baseline_rtt_floor_ms: 5.0,
            degrade_bad_samples: 3,
            degrade_min_spacing_ms: 500,
            degrade_rtt_factor: 2.0,
            degrade_loss_ppm: 20_000,
            degrade_stall_ms: 2_000,
            recover_good_samples: 5,
            recover_rtt_factor: 1.2,
            recover_loss_ppm: 5_000,
            recover_abs_rtt_ms: 120.0,
            min_switch_interval_ms: 30_000,
            observe_window_ms: 5_000,
            rollback_bad_samples: 3,
            rollback_span_ms: 1_500,
            backoff_ms: 5_000,
            probe_interval_ms: 1_000,
            probe_stale_ms: 2_000,
            switch_timeout_ms: 5_000,
        }
    }
}

impl PathConfig {
    /// 修正非法配置：容量/条数类先抬到 1；系数非有限或越界时回退默认值；
    /// 丢包 ppm 截断到 100 万；探测间隔与切换超时抬到 1ms 以免 0 值造成忙等语义。
    ///
    /// 该函数是幂等的，[`PathConfig::is_sane`] 等价于 `sanitized() == self`。
    pub fn sanitized(&self) -> Self {
        let mut cfg = self.clone();
        cfg.window_capacity = cfg.window_capacity.max(1);
        if !valid_alpha(cfg.ewma_alpha) {
            cfg.ewma_alpha = 0.2;
        }
        cfg.baseline_samples = cfg.baseline_samples.max(1);
        if !cfg.baseline_rtt_floor_ms.is_finite() || cfg.baseline_rtt_floor_ms < 0.0 {
            cfg.baseline_rtt_floor_ms = 5.0;
        }
        cfg.degrade_bad_samples = cfg.degrade_bad_samples.max(1);
        if !cfg.degrade_rtt_factor.is_finite() || cfg.degrade_rtt_factor <= 1.0 {
            cfg.degrade_rtt_factor = 2.0;
        }
        cfg.degrade_loss_ppm = cfg.degrade_loss_ppm.min(MAX_LOSS_PPM);
        cfg.recover_good_samples = cfg.recover_good_samples.max(1);
        if !cfg.recover_rtt_factor.is_finite() || cfg.recover_rtt_factor < 1.0 {
            cfg.recover_rtt_factor = 1.2;
        }
        cfg.recover_loss_ppm = cfg.recover_loss_ppm.min(MAX_LOSS_PPM);
        if !cfg.recover_abs_rtt_ms.is_finite() || cfg.recover_abs_rtt_ms <= 0.0 {
            cfg.recover_abs_rtt_ms = 120.0;
        }
        cfg.rollback_bad_samples = cfg.rollback_bad_samples.max(1);
        cfg.probe_interval_ms = cfg.probe_interval_ms.max(1);
        cfg.switch_timeout_ms = cfg.switch_timeout_ms.max(1);
        cfg
    }

    /// 配置是否已经合法（`sanitized()` 不改变任何字段）。
    pub fn is_sane(&self) -> bool {
        self.sanitized() == *self
    }
}

fn valid_alpha(alpha: f32) -> bool {
    alpha.is_finite() && alpha > 0.0 && alpha <= 1.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane_and_match_spec() {
        let cfg = PathConfig::default();
        assert!(cfg.is_sane());
        assert_eq!(cfg.window_capacity, 32);
        assert!((cfg.ewma_alpha - 0.2).abs() < f32::EPSILON);
        assert_eq!(cfg.baseline_samples, 5);
        assert_eq!(cfg.degrade_bad_samples, 3);
        assert_eq!(cfg.degrade_min_spacing_ms, 500);
        assert!((cfg.degrade_rtt_factor - 2.0).abs() < f32::EPSILON);
        assert_eq!(cfg.degrade_loss_ppm, 20_000);
        assert_eq!(cfg.degrade_stall_ms, 2_000);
        assert_eq!(cfg.recover_good_samples, 5);
        assert!((cfg.recover_rtt_factor - 1.2).abs() < f32::EPSILON);
        assert_eq!(cfg.recover_loss_ppm, 5_000);
        assert_eq!(cfg.min_switch_interval_ms, 30_000);
        assert_eq!(cfg.observe_window_ms, 5_000);
        assert_eq!(cfg.rollback_bad_samples, 3);
        assert_eq!(cfg.rollback_span_ms, 1_500);
        assert_eq!(cfg.backoff_ms, 5_000);
        assert_eq!(cfg.switch_timeout_ms, 5_000);
    }

    #[test]
    fn sanitize_repairs_invalid_values_and_is_idempotent() {
        let broken = PathConfig {
            window_capacity: 0,
            ewma_alpha: f32::NAN,
            baseline_samples: 0,
            baseline_rtt_floor_ms: f32::INFINITY,
            degrade_bad_samples: 0,
            degrade_rtt_factor: 1.0,
            degrade_loss_ppm: u32::MAX,
            recover_good_samples: 0,
            recover_rtt_factor: 0.5,
            recover_loss_ppm: u32::MAX,
            recover_abs_rtt_ms: -1.0,
            rollback_bad_samples: 0,
            probe_interval_ms: 0,
            switch_timeout_ms: 0,
            ..PathConfig::default()
        };
        assert!(!broken.is_sane());
        let fixed = broken.sanitized();
        assert!(fixed.is_sane());
        assert_eq!(fixed.window_capacity, 1);
        assert_eq!(fixed.baseline_samples, 1);
        assert_eq!(fixed.degrade_bad_samples, 1);
        assert_eq!(fixed.recover_good_samples, 1);
        assert_eq!(fixed.rollback_bad_samples, 1);
        assert_eq!(fixed.degrade_loss_ppm, MAX_LOSS_PPM);
        assert_eq!(fixed.recover_loss_ppm, MAX_LOSS_PPM);
        assert_eq!(fixed.probe_interval_ms, 1);
        assert_eq!(fixed.switch_timeout_ms, 1);
        assert!((fixed.ewma_alpha - 0.2).abs() < f32::EPSILON);
        assert_eq!(fixed, fixed.sanitized());
    }
}
