// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 路径切换状态机（纯逻辑）。
//!
//! [`PathMonitor`] 把一路路 [`PathSample`] 变成 [`Action`] 与 [`PathEvent`]：
//!
//! - 起点默认 `Direct`（业主决策：P2P 优先），也可用
//!   [`PathMonitor::with_initial_path`] 从 `Relay` 起步；
//! - `Direct` 平稳运行：先用健康采样建立 RTT 基线，之后按「连续 3 个坏采样
//!   且间隔 ≥500ms」判定劣化，要求切到 `Relay`；
//! - `Relay` 平稳运行：不再用 RTT 判劣化（中继天然 RTT 更高），只告警；同时按
//!   1s 节拍请求 P2P 探测，探测连续 5 个好采样且距上次切换 ≥30s 后切回；
//! - 切换提交后进入 5s **观察期**：新路径在 1.5s 内出现 3 个坏采样即回滚，
//!   并进入 5s **退避**（期间不再切换）；
//! - 切换请求 5s 内未被回填结果视为失败（与 `SessionResume` 的等待上限一致），
//!   同样回退并退避。
//!
//! 状态机不读系统时钟：所有时间来自采样的 `at_ms`（调用方单调时钟）；
//! 同样的输入序列必然得到同样的输出，便于无网络单测。
//!
//! # 最小集成流程
//!
//! ```
//! use dfy_path::{Action, PathConfig, PathKind, PathMonitor, PathSample};
//!
//! let mut monitor = PathMonitor::new(PathConfig::default());
//! // 前 5 个好采样建立基线（示例省略逐条喂入）。
//! for i in 0..5u64 {
//!     let good = PathSample {
//!         rtt_ms: 20.0,
//!         loss_ppm: 0,
//!         recv_bps: 8_000_000,
//!         stall_ms: 0,
//!         at_ms: 1_000 + i * 250,
//!     };
//!     assert_eq!(monitor.on_sample(good), Action::None);
//! }
//! // 之后出现持续劣化，状态机给出「切到 Relay」的指令。
//! let bad = PathSample {
//!     rtt_ms: 90.0,
//!     loss_ppm: 0,
//!     recv_bps: 0,
//!     stall_ms: 0,
//!     at_ms: 2_250,
//! };
//! assert_eq!(monitor.on_sample(bad), Action::None);
//! let bad = PathSample { at_ms: 2_750, ..bad };
//! assert_eq!(monitor.on_sample(bad), Action::None);
//! let bad = PathSample { at_ms: 3_250, ..bad };
//! assert_eq!(
//!     monitor.on_sample(bad),
//!     Action::SwitchTo(PathKind::Relay)
//! );
//! // 调用方完成「新连接 + 握手 + SessionResume」后回填结果：
//! // monitor.on_switch_committed(at_ms, took_ms);
//! // 失败则 monitor.on_switch_failed(at_ms);
//! ```

use std::mem;

use crate::config::PathConfig;
use crate::sample::{PathSample, SampleWindow, WindowStats};
use crate::state::{
    Action, Baseline, DegradeReason, PathEvent, PathKind, PathState, PathStats, PendingSwitch,
    Phase, SwitchDirection,
};

/// 事件缓冲区上限；调用方长期不 `drain` 时丢弃最旧事件，避免无界增长。
const MAX_BUFFERED_EVENTS: usize = 256;

/// 连续采样计数器。
#[derive(Debug, Clone, Copy, Default)]
struct Streak {
    count: u32,
    first_at_ms: u64,
    last_at_ms: u64,
}

impl Streak {
    fn reset(&mut self) {
        *self = Self::default();
    }

    /// 记录一个「坏」采样，但要求与上一个计入的坏采样间隔 ≥ `spacing_ms`。
    ///
    /// 间隔不足的坏采样被忽略（不清零）：250ms 采样节拍下，3 个坏采样约 1.25s
    /// 才触发切换，而不是 0.5s 的瞬时毛刺。
    fn record_spaced(&mut self, at_ms: u64, spacing_ms: u64) -> u32 {
        if self.count == 0 {
            self.count = 1;
            self.first_at_ms = at_ms;
            self.last_at_ms = at_ms;
        } else if at_ms.saturating_sub(self.last_at_ms) >= spacing_ms {
            self.count = self.count.saturating_add(1);
            self.last_at_ms = at_ms;
        }
        self.count
    }

    /// 记录一个观察期内的坏采样：只统计落在同一 `span_ms` 时间窗内的连续坏采样。
    /// 距首个坏采样超过 `span_ms` 时，以当前采样重新开窗。
    fn record_within(&mut self, at_ms: u64, span_ms: u64) -> u32 {
        if self.count == 0 || at_ms.saturating_sub(self.first_at_ms) > span_ms {
            self.count = 1;
            self.first_at_ms = at_ms;
            self.last_at_ms = at_ms;
        } else {
            self.count = self.count.saturating_add(1);
            self.last_at_ms = at_ms;
        }
        self.count
    }
}

/// 路径质量监测与切换决策状态机。
///
/// 一个实例监测「当前路径 + 一条候选路径」；两条路径的采样都通过
/// [`PathMonitor::on_sample_for`] 喂入，当前路径可省略路径参数。
#[derive(Debug, Clone)]
pub struct PathMonitor {
    config: PathConfig,
    state: PathState,
    stats: PathStats,
    direct: SampleWindow,
    relay: SampleWindow,
    events: Vec<PathEvent>,
    /// Direct 路径上的连续劣化计数。
    degrade: Streak,
    /// Relay 路径上连续「好探测」计数。
    recover_good: u32,
    /// 观察期内的连续坏采样计数。
    observe_bad: Streak,
    /// Relay 自身的连续劣化计数（只告警，不切换）。
    relay_bad: Streak,
    /// 建立 Direct 基线的预热采样。
    baseline_warmup: Vec<PathSample>,
    /// 最近一次 `RequestProbe(Direct)` 的时间。
    last_probe_at_ms: Option<u64>,
    /// 观察期回滚的目标（= 切换前的路径）。
    rollback_to: Option<PathKind>,
}

impl PathMonitor {
    /// 以 `Direct` 为初始路径创建状态机；配置会先经过
    /// [`PathConfig::sanitized`]，因此不会 panic。
    pub fn new(config: PathConfig) -> Self {
        Self::with_initial_path(config, PathKind::Direct)
    }

    /// 指定初始路径创建状态机（例如首次建连就失败、只能走中继时传 `Relay`）。
    pub fn with_initial_path(config: PathConfig, initial: PathKind) -> Self {
        let config = config.sanitized();
        let direct = SampleWindow::new(config.window_capacity, config.ewma_alpha);
        let relay = direct.clone();
        Self {
            state: PathState {
                active: initial,
                phase: Phase::Stable,
                baseline: None,
                pending: None,
            },
            stats: PathStats {
                samples: 0,
                switches: 0,
                rollbacks: 0,
                last_switch_at_ms: None,
                current: initial,
            },
            config,
            direct,
            relay,
            events: Vec::new(),
            degrade: Streak::default(),
            recover_good: 0,
            observe_bad: Streak::default(),
            relay_bad: Streak::default(),
            baseline_warmup: Vec::new(),
            last_probe_at_ms: None,
            rollback_to: None,
        }
    }

    /// 生效的配置（已 `sanitized`）。
    pub fn config(&self) -> &PathConfig {
        &self.config
    }

    /// 状态快照。
    pub fn state(&self) -> &PathState {
        &self.state
    }

    /// 累计统计。
    pub fn stats(&self) -> &PathStats {
        &self.stats
    }

    /// 当前使用的路径。
    pub fn active(&self) -> PathKind {
        self.state.active
    }

    /// P2P 基线（尚未建立时为 `None`）。
    pub fn baseline(&self) -> Option<Baseline> {
        self.state.baseline
    }

    /// 已请求、尚未回填结果的切换。
    pub fn pending_switch(&self) -> Option<PendingSwitch> {
        self.state.pending
    }

    /// 某条路径的滑动窗口（只读）。
    pub fn window(&self, path: PathKind) -> &SampleWindow {
        match path {
            PathKind::Direct => &self.direct,
            PathKind::Relay => &self.relay,
        }
    }

    /// 某条路径的窗口聚合；该路径还没有采样时为 `None`。
    pub fn window_stats(&self, path: PathKind) -> Option<WindowStats> {
        self.window(path).stats()
    }

    /// 已产生但未取走的事件。
    pub fn events(&self) -> &[PathEvent] {
        &self.events
    }

    /// 取走全部事件（指标上报/日志落盘用），之后 `events()` 为空。
    pub fn drain_events(&mut self) -> Vec<PathEvent> {
        mem::take(&mut self.events)
    }

    /// 喂入当前路径的一条采样，返回状态机指令。
    pub fn on_sample(&mut self, sample: PathSample) -> Action {
        let active = self.state.active;
        self.on_sample_for(active, sample)
    }

    /// 喂入指定路径的一条采样（候选路径的探测采样也走这里）。
    pub fn on_sample_for(&mut self, path: PathKind, sample: PathSample) -> Action {
        let sample = sample.clamped();
        // 探测采样的间隔检测需要「上一条」Direct 采样的时间（push 之后就变成当前这条了）。
        let previous_direct_at = if path == PathKind::Direct {
            self.direct.latest().map(|sample| sample.at_ms)
        } else {
            None
        };
        self.stats.samples = self.stats.samples.saturating_add(1);
        self.window_mut(path).push(sample);
        self.advance(sample.at_ms);
        if path == PathKind::Direct {
            self.maybe_capture_baseline(&sample);
        }
        // 切换进行中/退避中不做任何新决策，只继续记录采样。
        if matches!(
            self.state.phase,
            Phase::Switching { .. } | Phase::Backoff { .. }
        ) {
            return Action::None;
        }
        if path != self.state.active {
            return if self.state.active == PathKind::Relay && path == PathKind::Direct {
                self.on_direct_probe(&sample, previous_direct_at)
            } else {
                Action::None
            };
        }
        match self.state.phase {
            Phase::Observing { .. } => self.on_observing_sample(path, &sample),
            _ => match path {
                PathKind::Direct => self.on_direct_active(&sample),
                PathKind::Relay => self.on_relay_active(&sample),
            },
        }
    }

    /// 回填「切换成功」：新路径已建连并完成 `SessionResume`。
    ///
    /// 返回 `false` 表示当前没有待完成的切换（例如已超时被判失败），调用方应
    /// 关掉旧/新连接并继续使用 [`PathMonitor::active`]。
    pub fn on_switch_committed(&mut self, at_ms: u64, took_ms: u64) -> bool {
        let pending = match self.state.pending {
            Some(pending) => pending,
            None => return false,
        };
        self.state.active = pending.to;
        self.stats.current = pending.to;
        self.stats.switches = self.stats.switches.saturating_add(1);
        self.stats.last_switch_at_ms = Some(at_ms);
        self.window_mut(pending.to).reset();
        self.clear_decision_state();
        self.rollback_to = Some(pending.from);
        if self.state.baseline.is_none() {
            self.baseline_warmup.clear();
        }
        self.state.phase = Phase::Observing { since_ms: at_ms };
        self.state.pending = None;
        self.push_event(PathEvent::SwitchCommitted {
            to: pending.to,
            took_ms,
        });
        true
    }

    /// 回填「切换失败」（建连失败、握手失败、`SessionResume` 超时等）：
    /// 回到原路径并进入退避。返回 `false` 表示没有进行中的切换。
    pub fn on_switch_failed(&mut self, at_ms: u64) -> bool {
        self.fail_switch(at_ms)
    }

    // ---- 内部实现 ----

    fn window_mut(&mut self, path: PathKind) -> &mut SampleWindow {
        match path {
            PathKind::Direct => &mut self.direct,
            PathKind::Relay => &mut self.relay,
        }
    }

    fn push_event(&mut self, event: PathEvent) {
        if self.events.len() >= MAX_BUFFERED_EVENTS {
            self.events.remove(0);
        }
        self.events.push(event);
    }

    /// 时间驱动的阶段迁移：切换超时、观察期结束、退避结束、探测过期。
    fn advance(&mut self, at_ms: u64) {
        let phase = self.state.phase;
        match phase {
            Phase::Switching { started_at_ms, .. } => {
                if at_ms.saturating_sub(started_at_ms) >= self.config.switch_timeout_ms {
                    self.fail_switch(at_ms);
                }
            }
            Phase::Observing { since_ms } => {
                if at_ms.saturating_sub(since_ms) >= self.config.observe_window_ms {
                    self.state.phase = Phase::Stable;
                    self.observe_bad.reset();
                }
            }
            Phase::Backoff { until_ms } => {
                if at_ms >= until_ms {
                    self.state.phase = Phase::Stable;
                }
            }
            Phase::Probing { since_ms } => {
                if at_ms.saturating_sub(since_ms) >= self.config.probe_stale_ms {
                    self.state.phase = Phase::Stable;
                }
            }
            Phase::Stable => {}
        }
    }

    /// 切换请求超时/失败：回到原路径、进入退避、记一次回滚。
    fn fail_switch(&mut self, at_ms: u64) -> bool {
        let direction = match self.state.phase {
            Phase::Switching { direction, .. } => direction,
            _ => return false,
        };
        let from = direction.from();
        self.state.active = from;
        self.stats.current = from;
        self.stats.rollbacks = self.stats.rollbacks.saturating_add(1);
        self.stats.last_switch_at_ms = Some(at_ms);
        self.state.pending = None;
        self.state.phase = Phase::Backoff {
            until_ms: at_ms.saturating_add(self.config.backoff_ms),
        };
        self.rollback_to = None;
        self.clear_decision_state();
        self.push_event(PathEvent::RolledBack { to: from });
        true
    }

    fn clear_decision_state(&mut self) {
        self.degrade.reset();
        self.recover_good = 0;
        self.observe_bad.reset();
        self.relay_bad.reset();
        self.last_probe_at_ms = None;
    }

    /// 用健康采样建立 Direct 基线：有效 RTT、且 stall 不超过
    /// [`PathConfig::baseline_max_stall_ms`]，凑满 [`PathConfig::baseline_samples`]
    /// 条后取 RTT 均值 / 丢包均值 / 最大 stall。
    fn maybe_capture_baseline(&mut self, sample: &PathSample) {
        if self.state.baseline.is_some() {
            return;
        }
        if !sample.has_rtt() || sample.stall_ms > self.config.baseline_max_stall_ms {
            return;
        }
        self.baseline_warmup.push(*sample);
        if self.baseline_warmup.len() < self.config.baseline_samples {
            return;
        }
        let count = self.baseline_warmup.len() as f32;
        let mut rtt_sum = 0.0f32;
        let mut loss_sum = 0u64;
        let mut stall_max = 0u64;
        for warm in &self.baseline_warmup {
            rtt_sum += warm.rtt_ms;
            loss_sum += u64::from(warm.loss_ppm);
            stall_max = stall_max.max(warm.stall_ms);
        }
        self.state.baseline = Some(Baseline {
            rtt_ms: rtt_sum / count,
            loss_ppm: (loss_sum / self.baseline_warmup.len() as u64) as u32,
            stall_ms: stall_max,
            established_at_ms: sample.at_ms,
        });
        self.baseline_warmup.clear();
    }

    /// Direct 路径是否劣化；返回首个命中的原因（stall → 丢包 → RTT）。
    fn direct_bad_reason(&self, sample: &PathSample) -> Option<DegradeReason> {
        let cfg = &self.config;
        let baseline_stalled = match self.state.baseline {
            Some(baseline) => baseline.stall_ms > cfg.degrade_stall_ms,
            None => false,
        };
        if sample.stall_ms > cfg.degrade_stall_ms && !baseline_stalled {
            return Some(DegradeReason::Stall {
                stall_ms: sample.stall_ms,
                threshold_ms: cfg.degrade_stall_ms,
            });
        }
        if sample.loss_ppm > cfg.degrade_loss_ppm {
            return Some(DegradeReason::Loss {
                observed_ppm: sample.loss_ppm,
                threshold_ppm: cfg.degrade_loss_ppm,
            });
        }
        if !sample.has_rtt() {
            return None;
        }
        let baseline = self.state.baseline?;
        let reference = baseline.rtt_ms.max(cfg.baseline_rtt_floor_ms);
        if sample.rtt_ms > reference * cfg.degrade_rtt_factor {
            return Some(DegradeReason::Rtt {
                observed_ms: sample.rtt_ms,
                baseline_ms: baseline.rtt_ms,
                factor: cfg.degrade_rtt_factor,
            });
        }
        None
    }

    /// Relay 路径的劣化告警：只用丢包与 stall，不用 RTT（中继天然更慢）。
    fn relay_bad_reason(&self, sample: &PathSample) -> Option<DegradeReason> {
        let cfg = &self.config;
        if sample.stall_ms > cfg.degrade_stall_ms {
            return Some(DegradeReason::Stall {
                stall_ms: sample.stall_ms,
                threshold_ms: cfg.degrade_stall_ms,
            });
        }
        if sample.loss_ppm > cfg.degrade_loss_ppm {
            return Some(DegradeReason::Loss {
                observed_ppm: sample.loss_ppm,
                threshold_ppm: cfg.degrade_loss_ppm,
            });
        }
        None
    }

    /// Direct 探测采样是否「足够好」，可以计入恢复计数。
    ///
    /// 条件：有 RTT、丢包 ≤ [`PathConfig::recover_loss_ppm`]、stall ≤ 劣化阈值，
    /// 且 RTT ≤ `基线×1.2`（没有基线时用 [`PathConfig::recover_abs_rtt_ms`]）。
    fn direct_probe_good(&self, sample: &PathSample) -> bool {
        let cfg = &self.config;
        if !sample.has_rtt() {
            return false;
        }
        if sample.loss_ppm > cfg.recover_loss_ppm || sample.stall_ms > cfg.degrade_stall_ms {
            return false;
        }
        match self.state.baseline {
            Some(baseline) => {
                let reference = baseline.rtt_ms.max(cfg.baseline_rtt_floor_ms);
                sample.rtt_ms <= reference * cfg.recover_rtt_factor
            }
            None => sample.rtt_ms <= cfg.recover_abs_rtt_ms,
        }
    }

    /// 距上次切换是否已过冷期（[`PathConfig::min_switch_interval_ms`]）。
    fn cooldown_elapsed(&self, at_ms: u64) -> bool {
        match self.stats.last_switch_at_ms {
            Some(last) => at_ms.saturating_sub(last) >= self.config.min_switch_interval_ms,
            None => true,
        }
    }

    /// Direct 路径（当前激活）的常规劣化判定。
    fn on_direct_active(&mut self, sample: &PathSample) -> Action {
        let reason = match self.direct_bad_reason(sample) {
            Some(reason) => reason,
            None => {
                self.degrade.reset();
                return Action::None;
            }
        };
        let count = self
            .degrade
            .record_spaced(sample.at_ms, self.config.degrade_min_spacing_ms);
        if count < self.config.degrade_bad_samples {
            return Action::None;
        }
        self.push_event(PathEvent::Deteriorated {
            path: PathKind::Direct,
            reason,
        });
        self.push_event(PathEvent::SwitchRequested {
            from: PathKind::Direct,
            to: PathKind::Relay,
        });
        self.state.phase = Phase::Switching {
            direction: SwitchDirection::ToRelay,
            started_at_ms: sample.at_ms,
        };
        self.state.pending = Some(PendingSwitch {
            from: PathKind::Direct,
            to: PathKind::Relay,
            requested_at_ms: sample.at_ms,
        });
        self.degrade.reset();
        Action::SwitchTo(PathKind::Relay)
    }

    /// Relay 路径（当前激活）的告警与「探测 P2P」节拍。
    fn on_relay_active(&mut self, sample: &PathSample) -> Action {
        if let Some(reason) = self.relay_bad_reason(sample) {
            let count = self
                .relay_bad
                .record_spaced(sample.at_ms, self.config.degrade_min_spacing_ms);
            if count >= self.config.degrade_bad_samples {
                self.push_event(PathEvent::Deteriorated {
                    path: PathKind::Relay,
                    reason,
                });
                self.relay_bad.reset();
            }
        } else {
            self.relay_bad.reset();
        }
        if !self.cooldown_elapsed(sample.at_ms) {
            self.recover_good = 0;
            return Action::None;
        }
        let age = self.direct_age_ms(sample.at_ms);
        let stale = match age {
            Some(age) => age > self.config.probe_stale_ms,
            None => true,
        };
        if stale {
            // 探测长期没有回报：断链，恢复计数从头开始。
            self.recover_good = 0;
        }
        let need_probe = match age {
            Some(age) => age >= self.config.probe_interval_ms,
            None => true,
        };
        if !need_probe {
            return Action::None;
        }
        let probe_allowed = match self.last_probe_at_ms {
            Some(last) => sample.at_ms.saturating_sub(last) >= self.config.probe_interval_ms,
            None => true,
        };
        if !probe_allowed {
            return Action::None;
        }
        self.last_probe_at_ms = Some(sample.at_ms);
        self.state.phase = Phase::Probing {
            since_ms: sample.at_ms,
        };
        Action::RequestProbe(PathKind::Direct)
    }

    /// Relay 激活时收到 Direct 探测采样：累计连续好采样，够数就请求切回。
    ///
    /// `previous_at` 是这条采样之前、Direct 窗口里的最新时间戳；两条探测间隔超过
    /// [`PathConfig::probe_stale_ms`] 说明链条断了，连续计数从头开始。
    fn on_direct_probe(&mut self, sample: &PathSample, previous_at: Option<u64>) -> Action {
        if let Some(previous_at) = previous_at {
            if sample.at_ms.saturating_sub(previous_at) > self.config.probe_stale_ms {
                self.recover_good = 0;
            }
        }
        match self.state.phase {
            Phase::Stable | Phase::Probing { .. } => {}
            _ => return Action::None,
        }
        if !self.cooldown_elapsed(sample.at_ms) {
            self.recover_good = 0;
            return Action::None;
        }
        if !self.direct_probe_good(sample) {
            self.recover_good = 0;
            return Action::None;
        }
        self.recover_good = self.recover_good.saturating_add(1);
        if self.recover_good < self.config.recover_good_samples {
            return Action::None;
        }
        self.push_event(PathEvent::Recovered);
        self.push_event(PathEvent::SwitchRequested {
            from: PathKind::Relay,
            to: PathKind::Direct,
        });
        self.state.phase = Phase::Switching {
            direction: SwitchDirection::ToDirect,
            started_at_ms: sample.at_ms,
        };
        self.state.pending = Some(PendingSwitch {
            from: PathKind::Relay,
            to: PathKind::Direct,
            requested_at_ms: sample.at_ms,
        });
        self.recover_good = 0;
        Action::SwitchTo(PathKind::Direct)
    }

    /// 观察期：新路径立即劣化（时间窗内连续坏采样）则回滚到上一路径。
    fn on_observing_sample(&mut self, path: PathKind, sample: &PathSample) -> Action {
        let reason = match path {
            PathKind::Direct => self.direct_bad_reason(sample),
            PathKind::Relay => self.relay_bad_reason(sample),
        };
        let reason = match reason {
            Some(reason) => reason,
            None => {
                self.observe_bad.reset();
                return Action::None;
            }
        };
        let count = self
            .observe_bad
            .record_within(sample.at_ms, self.config.rollback_span_ms);
        if count < self.config.rollback_bad_samples {
            return Action::None;
        }
        let target = match self.rollback_to {
            Some(target) => target,
            None => path.other(),
        };
        self.push_event(PathEvent::Deteriorated { path, reason });
        self.push_event(PathEvent::RolledBack { to: target });
        self.state.active = target;
        self.stats.current = target;
        self.stats.rollbacks = self.stats.rollbacks.saturating_add(1);
        self.stats.last_switch_at_ms = Some(sample.at_ms);
        self.state.pending = None;
        self.state.phase = Phase::Backoff {
            until_ms: sample.at_ms.saturating_add(self.config.backoff_ms),
        };
        self.rollback_to = None;
        self.window_mut(target).reset();
        self.clear_decision_state();
        Action::SwitchTo(target)
    }

    /// Direct 最新采样的年龄（毫秒）；没有采样时为 `None`。
    fn direct_age_ms(&self, at_ms: u64) -> Option<u64> {
        self.direct
            .latest()
            .map(|sample| at_ms.saturating_sub(sample.at_ms))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(at_ms: u64, rtt_ms: f32, loss_ppm: u32, stall_ms: u64) -> PathSample {
        PathSample {
            rtt_ms,
            loss_ppm,
            recv_bps: 8_000_000,
            stall_ms,
            at_ms,
        }
    }

    fn good(at_ms: u64) -> PathSample {
        sample(at_ms, 20.0, 0, 0)
    }

    fn new_monitor() -> PathMonitor {
        PathMonitor::new(PathConfig::default())
    }

    /// 喂入 5 个健康采样（1000..2000，每 250ms），返回最后一个时间戳。
    fn warmup(monitor: &mut PathMonitor) -> u64 {
        let mut last = 0;
        for i in 0..5u64 {
            last = 1_000 + i * 250;
            assert_eq!(monitor.on_sample(good(last)), Action::None);
        }
        assert!(monitor.baseline().is_some());
        last
    }

    /// 预热后连续 3 个间隔 500ms 的坏 RTT 采样，停在「已请求切到 Relay」。
    fn degraded_monitor() -> PathMonitor {
        let mut monitor = new_monitor();
        warmup(&mut monitor);
        assert_eq!(monitor.on_sample(sample(2_250, 45.0, 0, 0)), Action::None);
        assert_eq!(monitor.on_sample(sample(2_750, 45.0, 0, 0)), Action::None);
        assert_eq!(
            monitor.on_sample(sample(3_250, 45.0, 0, 0)),
            Action::SwitchTo(PathKind::Relay)
        );
        monitor
    }

    /// 已切到 Relay 并处于观察期。
    fn relay_observing_monitor() -> PathMonitor {
        let mut monitor = degraded_monitor();
        assert!(monitor.on_switch_committed(3_750, 220));
        monitor.drain_events();
        monitor
    }

    #[test]
    fn fresh_monitor_starts_on_direct_and_stable() {
        let monitor = new_monitor();
        assert_eq!(monitor.active(), PathKind::Direct);
        assert_eq!(monitor.stats().current, PathKind::Direct);
        assert_eq!(monitor.stats().samples, 0);
        assert_eq!(monitor.stats().switches, 0);
        assert_eq!(monitor.stats().last_switch_at_ms, None);
        assert!(matches!(monitor.state().phase, Phase::Stable));
        assert!(monitor.baseline().is_none());
        assert!(monitor.pending_switch().is_none());
        assert!(monitor.events().is_empty());
    }

    #[test]
    fn with_initial_path_can_start_on_relay() {
        let mut monitor = PathMonitor::with_initial_path(PathConfig::default(), PathKind::Relay);
        assert_eq!(monitor.active(), PathKind::Relay);
        assert_eq!(monitor.stats().current, PathKind::Relay);
        // Relay 起步时没有 P2P 采样，首个 Relay 采样即请求探测。
        assert_eq!(
            monitor.on_sample_for(PathKind::Relay, good(1_000)),
            Action::RequestProbe(PathKind::Direct)
        );
        assert_eq!(monitor.window(PathKind::Direct).len(), 0);
        assert_eq!(monitor.window(PathKind::Relay).len(), 1);
    }

    #[test]
    fn steady_good_samples_build_baseline_without_switching() {
        let mut monitor = new_monitor();
        warmup(&mut monitor);
        let baseline = monitor.baseline().unwrap();
        assert!((baseline.rtt_ms - 20.0).abs() < 0.001);
        assert_eq!(baseline.stall_ms, 0);
        assert_eq!(baseline.established_at_ms, 2_000);
        for i in 0..20u64 {
            assert_eq!(monitor.on_sample(good(2_250 + i * 250)), Action::None);
        }
        assert_eq!(monitor.stats().switches, 0);
        assert!(monitor.events().is_empty());
    }

    #[test]
    fn p2p_degrade_switches_to_relay_after_three_spaced_bad_samples() {
        let monitor = degraded_monitor();
        assert_eq!(monitor.active(), PathKind::Direct);
        assert!(matches!(
            monitor.state().phase,
            Phase::Switching {
                direction: SwitchDirection::ToRelay,
                started_at_ms: 3_250
            }
        ));
        assert_eq!(
            monitor.pending_switch(),
            Some(PendingSwitch {
                from: PathKind::Direct,
                to: PathKind::Relay,
                requested_at_ms: 3_250
            })
        );
        let events = monitor.events();
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events[0],
            PathEvent::Deteriorated {
                path: PathKind::Direct,
                reason: DegradeReason::Rtt { .. }
            }
        ));
        assert_eq!(
            events[1],
            PathEvent::SwitchRequested {
                from: PathKind::Direct,
                to: PathKind::Relay
            }
        );
    }

    #[test]
    fn p2p_degrade_ignores_bursty_bad_samples() {
        let mut monitor = new_monitor();
        warmup(&mut monitor);
        // 每 100ms 一个坏采样：间隔 <500ms 的不计数，需要 ~1s 才凑够 3 个。
        for at in [
            2_100u64, 2_200, 2_300, 2_400, 2_500, 2_600, 2_700, 2_800, 2_900, 3_000,
        ] {
            assert_eq!(monitor.on_sample(sample(at, 60.0, 0, 0)), Action::None);
        }
        assert_eq!(
            monitor.on_sample(sample(3_100, 60.0, 0, 0)),
            Action::SwitchTo(PathKind::Relay)
        );
    }

    #[test]
    fn mild_rtt_spike_does_not_switch() {
        let mut monitor = new_monitor();
        warmup(&mut monitor);
        for i in 0..10u64 {
            // 35ms < 2×20ms，仍属健康范围。
            assert_eq!(
                monitor.on_sample(sample(2_250 + i * 500, 35.0, 0, 0)),
                Action::None
            );
        }
        assert_eq!(monitor.stats().switches, 0);
    }

    #[test]
    fn loss_and_stall_also_trigger_degrade() {
        let mut monitor = new_monitor();
        warmup(&mut monitor);
        for at in [2_250u64, 2_750, 3_250] {
            let action = monitor.on_sample(sample(at, 20.0, 30_000, 0));
            if at < 3_250 {
                assert_eq!(action, Action::None);
            } else {
                assert_eq!(action, Action::SwitchTo(PathKind::Relay));
            }
        }
        assert!(matches!(
            monitor.events()[0],
            PathEvent::Deteriorated {
                reason: DegradeReason::Loss { .. },
                ..
            }
        ));

        let mut monitor = new_monitor();
        warmup(&mut monitor);
        for at in [2_250u64, 2_750, 3_250] {
            let action = monitor.on_sample(sample(at, 20.0, 0, 2_500));
            if at < 3_250 {
                assert_eq!(action, Action::None);
            } else {
                assert_eq!(action, Action::SwitchTo(PathKind::Relay));
            }
        }
        assert!(matches!(
            monitor.events()[0],
            PathEvent::Deteriorated {
                reason: DegradeReason::Stall { .. },
                ..
            }
        ));
    }

    #[test]
    fn baseline_skips_stalled_samples() {
        let mut monitor = new_monitor();
        for i in 0..10u64 {
            // stall 1000ms > baseline_max_stall_ms(500ms)，不参与基线。
            assert_eq!(
                monitor.on_sample(sample(1_000 + i * 250, 20.0, 0, 1_000)),
                Action::None
            );
        }
        assert!(monitor.baseline().is_none());
        assert_eq!(monitor.stats().switches, 0);
    }

    #[test]
    fn commit_switches_path_and_enters_observing() {
        let mut monitor = relay_observing_monitor();
        assert_eq!(monitor.active(), PathKind::Relay);
        assert_eq!(monitor.stats().switches, 1);
        assert_eq!(monitor.stats().last_switch_at_ms, Some(3_750));
        assert!(matches!(
            monitor.state().phase,
            Phase::Observing { since_ms: 3_750 }
        ));
        assert!(monitor.pending_switch().is_none());
        // 新路径的窗口在提交时被清空（新连接 = 新的测量上下文）。
        assert_eq!(monitor.window(PathKind::Relay).len(), 0);
        monitor.on_sample_for(PathKind::Relay, good(4_000));
        assert_eq!(monitor.window(PathKind::Relay).len(), 1);
    }

    #[test]
    fn switch_committed_event_reports_elapsed_time() {
        let mut monitor = degraded_monitor();
        assert!(monitor.on_switch_committed(3_750, 260));
        let events = monitor.drain_events();
        assert_eq!(
            events.last(),
            Some(&PathEvent::SwitchCommitted {
                to: PathKind::Relay,
                took_ms: 260
            })
        );
    }

    #[test]
    fn observing_rolls_back_on_immediate_degradation() {
        let mut monitor = relay_observing_monitor();
        assert_eq!(
            monitor.on_sample_for(PathKind::Relay, sample(4_000, 60.0, 30_000, 0)),
            Action::None
        );
        assert_eq!(
            monitor.on_sample_for(PathKind::Relay, sample(4_250, 60.0, 30_000, 0)),
            Action::None
        );
        assert_eq!(
            monitor.on_sample_for(PathKind::Relay, sample(4_500, 60.0, 30_000, 0)),
            Action::SwitchTo(PathKind::Direct)
        );
        assert_eq!(monitor.active(), PathKind::Direct);
        assert_eq!(monitor.stats().rollbacks, 1);
        assert_eq!(monitor.stats().last_switch_at_ms, Some(4_500));
        assert!(matches!(
            monitor.state().phase,
            Phase::Backoff { until_ms: 9_500 }
        ));
        assert!(contains_event(&monitor, |event| matches!(
            event,
            PathEvent::RolledBack {
                to: PathKind::Direct
            }
        )));
    }

    #[test]
    fn observing_expires_back_to_stable() {
        let mut monitor = relay_observing_monitor();
        for i in 0..25u64 {
            assert_eq!(
                monitor.on_sample_for(PathKind::Relay, good(4_000 + i * 250)),
                Action::None
            );
        }
        assert!(matches!(monitor.state().phase, Phase::Stable));
    }

    #[test]
    fn healthy_probes_do_not_switch_back_before_cooldown() {
        let mut monitor = relay_observing_monitor();
        for i in 0..20u64 {
            assert_eq!(
                monitor.on_sample_for(PathKind::Relay, good(4_000 + i * 250)),
                Action::None
            );
        }
        // 距上次切换仅 ~11s < 30s 冷期，即使探测全好也不切回。
        for i in 0..6u64 {
            assert_eq!(
                monitor.on_sample_for(PathKind::Direct, good(10_000 + i * 1_000)),
                Action::None
            );
        }
        assert_eq!(monitor.active(), PathKind::Relay);
        assert_eq!(monitor.stats().switches, 1);
    }

    #[test]
    fn healthy_probes_switch_back_after_cooldown() {
        let mut monitor = relay_observing_monitor();
        let mut switched = false;
        for i in 0..6u64 {
            let at = 34_000 + i * 1_000;
            if monitor.on_sample_for(PathKind::Direct, good(at))
                == Action::SwitchTo(PathKind::Direct)
            {
                switched = true;
                assert!(matches!(
                    monitor.state().phase,
                    Phase::Switching {
                        direction: SwitchDirection::ToDirect,
                        started_at_ms
                    } if started_at_ms == at
                ));
                break;
            }
        }
        assert!(switched);
        assert!(contains_event(&monitor, |event| matches!(
            event,
            PathEvent::Recovered
        )));
    }

    #[test]
    fn bad_probe_resets_recover_streak() {
        let mut monitor = relay_observing_monitor();
        let base = 34_000;
        assert_eq!(
            monitor.on_sample_for(PathKind::Direct, good(base)),
            Action::None
        );
        assert_eq!(
            monitor.on_sample_for(PathKind::Direct, good(base + 1_000)),
            Action::None
        );
        // 坏探测把连续好采样清零。
        assert_eq!(
            monitor.on_sample_for(PathKind::Direct, sample(base + 2_000, 200.0, 0, 0)),
            Action::None
        );
        for i in 0..4u64 {
            assert_eq!(
                monitor.on_sample_for(PathKind::Direct, good(base + 3_000 + i * 1_000)),
                Action::None
            );
        }
        assert_eq!(
            monitor.on_sample_for(PathKind::Direct, good(base + 7_000)),
            Action::SwitchTo(PathKind::Direct)
        );
    }

    #[test]
    fn switch_failure_enters_backoff_and_blocks_switching() {
        let mut monitor = degraded_monitor();
        assert!(monitor.on_switch_failed(4_000));
        assert_eq!(monitor.active(), PathKind::Direct);
        assert_eq!(monitor.stats().rollbacks, 1);
        assert_eq!(monitor.stats().last_switch_at_ms, Some(4_000));
        assert!(matches!(
            monitor.state().phase,
            Phase::Backoff { until_ms: 9_000 }
        ));
        assert!(contains_event(&monitor, |event| matches!(
            event,
            PathEvent::RolledBack {
                to: PathKind::Direct
            }
        )));
        // 退避期内坏采样不触发切换。
        for at in [4_250u64, 4_750, 5_250, 5_750, 6_250] {
            assert_eq!(monitor.on_sample(sample(at, 60.0, 0, 0)), Action::None);
        }
        // 退避结束（9000ms）后重新计数，3 个间隔 500ms 的坏采样再次请求切换。
        let mut action = Action::None;
        for at in [9_000u64, 9_500, 10_000] {
            action = monitor.on_sample(sample(at, 60.0, 0, 0));
        }
        assert_eq!(action, Action::SwitchTo(PathKind::Relay));
    }

    #[test]
    fn switch_timeout_counts_as_failure() {
        let mut monitor = degraded_monitor();
        // 请求于 3250ms，超时上限 5000ms：8300ms 的采样触发超时回退。
        assert_eq!(monitor.on_sample(good(8_300)), Action::None);
        assert_eq!(monitor.active(), PathKind::Direct);
        assert_eq!(monitor.stats().rollbacks, 1);
        assert!(matches!(monitor.state().phase, Phase::Backoff { .. }));
        // 迟到的提交被拒绝，不得把状态机带偏。
        assert!(!monitor.on_switch_committed(8_400, 100));
        assert_eq!(monitor.active(), PathKind::Direct);
        assert_eq!(monitor.stats().switches, 0);
    }

    #[test]
    fn relay_degradation_emits_event_without_switching() {
        let mut monitor = relay_observing_monitor();
        for i in 0..21u64 {
            assert_eq!(
                monitor.on_sample_for(PathKind::Relay, good(4_000 + i * 250)),
                Action::None
            );
        }
        assert!(matches!(monitor.state().phase, Phase::Stable));
        for at in [9_250u64, 9_750, 10_250] {
            assert_eq!(
                monitor.on_sample_for(PathKind::Relay, sample(at, 80.0, 30_000, 0)),
                Action::None
            );
        }
        assert!(contains_event(&monitor, |event| matches!(
            event,
            PathEvent::Deteriorated {
                path: PathKind::Relay,
                reason: DegradeReason::Loss { .. }
            }
        )));
        assert_eq!(monitor.active(), PathKind::Relay);
    }

    #[test]
    fn stale_direct_requests_probe_on_relay() {
        let mut monitor = relay_observing_monitor();
        for i in 0..21u64 {
            let _ = monitor.on_sample_for(PathKind::Relay, good(4_000 + i * 250));
        }
        // 冷期已过（最后一次切换在 3750ms），Direct 无采样 → 请求探测。
        assert_eq!(
            monitor.on_sample_for(PathKind::Relay, good(34_000)),
            Action::RequestProbe(PathKind::Direct)
        );
        assert!(matches!(
            monitor.state().phase,
            Phase::Probing { since_ms: 34_000 }
        ));
        // 探测间隔 1s 内不重复请求。
        assert_eq!(
            monitor.on_sample_for(PathKind::Relay, good(34_250)),
            Action::None
        );
    }

    #[test]
    fn invalid_values_do_not_panic() {
        let mut monitor = PathMonitor::new(PathConfig {
            window_capacity: 0,
            ewma_alpha: f32::NAN,
            baseline_samples: 1,
            degrade_bad_samples: u32::MAX,
            ..PathConfig::default()
        });
        assert_eq!(monitor.window(PathKind::Direct).capacity(), 1);
        let weird = PathSample {
            rtt_ms: f32::NAN,
            loss_ppm: u32::MAX,
            recv_bps: u64::MAX,
            stall_ms: u64::MAX,
            at_ms: 0,
        };
        assert_eq!(monitor.on_sample(weird), Action::None);
        assert_eq!(
            monitor.on_sample(PathSample {
                rtt_ms: f32::INFINITY,
                ..weird
            }),
            Action::None
        );
        assert_eq!(
            monitor.on_sample(PathSample {
                rtt_ms: -1.0,
                at_ms: 100,
                ..weird
            }),
            Action::None
        );
        // 时间倒退不 panic。
        assert_eq!(
            monitor.on_sample(PathSample {
                rtt_ms: 20.0,
                at_ms: 50,
                ..weird
            }),
            Action::None
        );
        assert_eq!(monitor.stats().samples, 4);
        assert!(monitor.baseline().is_none());
    }

    #[test]
    fn probe_and_active_windows_are_tracked_separately() {
        let mut monitor = relay_observing_monitor();
        // 切换提交时只清空目标路径（Relay）窗口；Direct 窗口保留旧连接的历史。
        assert_eq!(monitor.window(PathKind::Relay).len(), 0);
        assert!(monitor.window_stats(PathKind::Relay).is_none());
        let direct_before = monitor.window(PathKind::Direct).len();
        let _ = monitor.on_sample_for(PathKind::Direct, sample(34_000, 20.0, 0, 0));
        assert_eq!(monitor.window(PathKind::Direct).len(), direct_before + 1);
        assert_eq!(monitor.window(PathKind::Relay).len(), 0);
        let stats = monitor.window_stats(PathKind::Direct).unwrap();
        assert_eq!(stats.samples, direct_before + 1);
        assert_eq!(stats.newest_at_ms, 34_000);
    }

    #[test]
    fn identical_inputs_produce_identical_actions() {
        fn run() -> (Vec<Action>, Vec<PathEvent>, PathStats) {
            let mut monitor = new_monitor();
            let mut actions = Vec::new();
            for i in 0..5u64 {
                actions.push(monitor.on_sample(good(1_000 + i * 250)));
            }
            for at in [2_250u64, 2_750, 3_250, 3_750, 4_000] {
                actions.push(monitor.on_sample(sample(at, 60.0, 0, 0)));
            }
            (actions, monitor.drain_events(), *monitor.stats())
        }
        let first = run();
        let second = run();
        assert_eq!(first, second);
        assert!(first.0.contains(&Action::SwitchTo(PathKind::Relay)));
        assert_eq!(first.2.switches, 0);
    }

    fn contains_event(monitor: &PathMonitor, predicate: impl Fn(&PathEvent) -> bool) -> bool {
        monitor.events().iter().any(predicate)
    }
}
