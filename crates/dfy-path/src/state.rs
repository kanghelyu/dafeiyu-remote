// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 状态机对外可见的类型：路径、阶段、动作、事件与统计。
//!
//! 这些类型都是纯数据（`Copy`/`Clone`），日志层可直接 `Display` 打印；
//! 不含任何 IO 或时钟。

use std::fmt;

/// 一条可用的网络路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PathKind {
    /// P2P 直连（NAT 打洞成功后的 UDP 路径，或局域网直连）。
    Direct,
    /// 经中继服务器转发（零知识 UDP 中转）。
    Relay,
}

impl PathKind {
    /// 另一条路径。
    pub fn other(self) -> Self {
        match self {
            Self::Direct => Self::Relay,
            Self::Relay => Self::Direct,
        }
    }

    /// 稳定短名（日志/指标 label 用）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Relay => "relay",
        }
    }
}

impl fmt::Display for PathKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Direct => "P2P",
            Self::Relay => "Relay",
        })
    }
}

/// 一次切换的方向（由 `from`/`to` 唯一确定）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchDirection {
    /// P2P 劣化 → 切到中继。
    ToRelay,
    /// 中继上确认 P2P 恢复 → 切回直连。
    ToDirect,
}

impl SwitchDirection {
    /// 切换起点。
    pub fn from(self) -> PathKind {
        match self {
            Self::ToRelay => PathKind::Direct,
            Self::ToDirect => PathKind::Relay,
        }
    }

    /// 切换目标。
    pub fn to(self) -> PathKind {
        match self {
            Self::ToDirect => PathKind::Direct,
            Self::ToRelay => PathKind::Relay,
        }
    }

    /// 由「从哪条路径切到哪条」构造；同路径返回 `None`。
    pub fn between(from: PathKind, to: PathKind) -> Option<Self> {
        match (from, to) {
            (PathKind::Direct, PathKind::Relay) => Some(Self::ToRelay),
            (PathKind::Relay, PathKind::Direct) => Some(Self::ToDirect),
            _ => None,
        }
    }
}

impl fmt::Display for SwitchDirection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} → {}", self.from(), self.to())
    }
}

/// 状态机的阶段。切换/回滚的「进行中」信息都在这里，调用方可直接展示。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// 平稳运行：按常规规则监测劣化（Direct）或探测/恢复（Relay）。
    Stable,
    /// 中继路径上已发出 `RequestProbe(Direct)`，等待 P2P 探测采样。
    Probing {
        /// 探测开始时间（毫秒）。
        since_ms: u64,
    },
    /// 已请求切换，等待调用方执行「新连接 + 握手 + SessionResume」后回填结果。
    Switching {
        /// 切换方向。
        direction: SwitchDirection,
        /// 请求时间（毫秒）。
        started_at_ms: u64,
    },
    /// 切换已提交，处于观察期（默认 5s）；期间只判「立即劣化」。
    Observing {
        /// 观察期起点（毫秒，即提交时间）。
        since_ms: u64,
    },
    /// 退避期：切换失败/观察期回滚后，在 `until_ms` 之前不再切换。
    Backoff {
        /// 退避结束时间（毫秒，含）。
        until_ms: u64,
    },
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stable => f.write_str("Stable"),
            Self::Probing { since_ms } => write!(f, "Probing(since {since_ms}ms)"),
            Self::Switching {
                direction,
                started_at_ms,
            } => write!(f, "Switching({direction}, started {started_at_ms}ms)"),
            Self::Observing { since_ms } => write!(f, "Observing(since {since_ms}ms)"),
            Self::Backoff { until_ms } => write!(f, "Backoff(until {until_ms}ms)"),
        }
    }
}

/// 切换请求的完整描述（[`PathState::pending`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingSwitch {
    /// 当前（旧）路径。
    pub from: PathKind,
    /// 目标路径。
    pub to: PathKind,
    /// 请求时间（毫秒）。
    pub requested_at_ms: u64,
}

/// P2P 直连的 RTT/丢包基线。由建立连接后的前 `baseline_samples` 个健康采样得到。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Baseline {
    /// 基线 RTT（毫秒，窗口均值）。
    pub rtt_ms: f32,
    /// 基线丢包率（ppm，窗口均值，仅诊断用）。
    pub loss_ppm: u32,
    /// 基线期间观察到的最大 stall（毫秒）；用于「基线未 stall」判定。
    pub stall_ms: u64,
    /// 基线建立时间（毫秒，最后一个基线采样的时间戳）。
    pub established_at_ms: u64,
}

/// 状态机的公开快照。
#[derive(Debug, Clone, PartialEq)]
pub struct PathState {
    /// 当前正在使用的路径。
    pub active: PathKind,
    /// 当前阶段。
    pub phase: Phase,
    /// P2P 基线；尚未建立（或从未在 P2P 上采集到健康采样）时为 `None`。
    pub baseline: Option<Baseline>,
    /// 已请求但尚未回填结果的切换。
    pub pending: Option<PendingSwitch>,
}

/// 状态机对调用方的指令。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// 什么都不用做。
    None,
    /// 请执行路径切换：新建目标路径的连接 + 握手 + `SessionResume` 续帧，
    /// 完成后调用 [`crate::PathMonitor::on_switch_committed`]（失败则
    /// [`crate::PathMonitor::on_switch_failed`]）。
    SwitchTo(PathKind),
    /// 请在目标路径上做一次轻量探测并回报采样（不切换连接）：
    /// `on_sample_for(PathKind::Direct, ...)`。
    RequestProbe(PathKind),
}

impl Action {
    /// 是否是「无动作」。
    pub fn is_none(self) -> bool {
        matches!(self, Self::None)
    }
}

/// 一次劣化判定的具体原因（用于事件与日志，避免只记「劣化」）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DegradeReason {
    /// RTT 超过基线的 `factor` 倍。
    Rtt {
        /// 观测 RTT（毫秒）。
        observed_ms: f32,
        /// 基线 RTT（毫秒，未含下限）。
        baseline_ms: f32,
        /// 判定倍率。
        factor: f32,
    },
    /// 丢包率超阈值。
    Loss {
        /// 观测丢包（ppm）。
        observed_ppm: u32,
        /// 阈值（ppm）。
        threshold_ppm: u32,
    },
    /// 视频「无进展」时长超阈值。
    Stall {
        /// 观测 stall（毫秒）。
        stall_ms: u64,
        /// 阈值（毫秒）。
        threshold_ms: u64,
    },
}

impl fmt::Display for DegradeReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rtt {
                observed_ms,
                baseline_ms,
                factor,
            } => write!(
                f,
                "RTT {observed_ms:.1}ms > {factor:.2}×基线 {baseline_ms:.1}ms"
            ),
            Self::Loss {
                observed_ppm,
                threshold_ppm,
            } => write!(
                f,
                "丢包 {:.2}% > {:.2}%",
                *observed_ppm as f64 / 10_000.0,
                *threshold_ppm as f64 / 10_000.0
            ),
            Self::Stall {
                stall_ms,
                threshold_ms,
            } => write!(f, "无进展 {stall_ms}ms > {threshold_ms}ms"),
        }
    }
}

/// 供调用方记录指标/日志的事件。由 [`crate::PathMonitor::drain_events`] 取出。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PathEvent {
    /// 判定某条路径劣化（Direct 上会随后请求切到 Relay；Relay 上只告警）。
    Deteriorated {
        /// 劣化的路径。
        path: PathKind,
        /// 具体原因。
        reason: DegradeReason,
    },
    /// 已向调用方发出切换请求（`Action::SwitchTo` 的伴生事件）。
    SwitchRequested {
        /// 起点路径。
        from: PathKind,
        /// 目标路径。
        to: PathKind,
    },
    /// 切换已提交成功（调用方回填 [`crate::PathMonitor::on_switch_committed`]）。
    SwitchCommitted {
        /// 目标路径。
        to: PathKind,
        /// 从请求到提交的耗时（毫秒）。
        took_ms: u64,
    },
    /// 回滚到上一路径（观察期劣化或切换失败/超时）。
    RolledBack {
        /// 回滚目标路径。
        to: PathKind,
    },
    /// P2P 质量恢复（切回 Direct 前发出）。
    Recovered,
}

impl fmt::Display for PathEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Deteriorated { path, reason } => write!(f, "[{path}] 劣化: {reason}"),
            Self::SwitchRequested { from, to } => write!(f, "请求切换 {from} → {to}"),
            Self::SwitchCommitted { to, took_ms } => {
                write!(f, "切换完成 → {to}（{took_ms}ms）")
            }
            Self::RolledBack { to } => write!(f, "回滚 → {to}"),
            Self::Recovered => f.write_str("P2P 质量恢复"),
        }
    }
}

/// 累计统计。`switches` 只计「提交成功」的切换；失败/回滚计入 `rollbacks`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathStats {
    /// 收到的采样总数（含探测采样与非法采样）。
    pub samples: u64,
    /// 成功提交的切换次数。
    pub switches: u32,
    /// 回滚与切换失败次数。
    pub rollbacks: u32,
    /// 最近一次切换/回滚的时间（毫秒）；从未发生时为 `None`。
    pub last_switch_at_ms: Option<u64>,
    /// 当前路径（[`PathState::active`] 的镜像，便于指标直接读取）。
    pub current: PathKind,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_kind_helpers_and_display() {
        assert_eq!(PathKind::Direct.other(), PathKind::Relay);
        assert_eq!(PathKind::Relay.other(), PathKind::Direct);
        assert_eq!(PathKind::Direct.as_str(), "direct");
        assert_eq!(PathKind::Relay.to_string(), "Relay");
    }

    #[test]
    fn switch_direction_maps_between_paths() {
        assert_eq!(
            SwitchDirection::between(PathKind::Direct, PathKind::Relay),
            Some(SwitchDirection::ToRelay)
        );
        assert_eq!(
            SwitchDirection::between(PathKind::Relay, PathKind::Direct),
            Some(SwitchDirection::ToDirect)
        );
        assert_eq!(
            SwitchDirection::between(PathKind::Direct, PathKind::Direct),
            None
        );
        let dir = SwitchDirection::ToDirect;
        assert_eq!(dir.from(), PathKind::Relay);
        assert_eq!(dir.to(), PathKind::Direct);
        assert_eq!(dir.to_string(), "Relay → P2P");
    }

    #[test]
    fn degrade_reason_display_contains_measurements() {
        let rtt = DegradeReason::Rtt {
            observed_ms: 55.0,
            baseline_ms: 20.0,
            factor: 2.0,
        };
        let text = rtt.to_string();
        assert!(text.contains("55.0"));
        assert!(text.contains("2.00"));
        let loss = DegradeReason::Loss {
            observed_ppm: 30_000,
            threshold_ppm: 20_000,
        };
        assert!(loss.to_string().contains("3.00%"));
        let stall = DegradeReason::Stall {
            stall_ms: 2_500,
            threshold_ms: 2_000,
        };
        assert!(stall.to_string().contains("2500ms"));
    }

    #[test]
    fn event_and_phase_display_are_readable() {
        assert!(PathEvent::Recovered.to_string().contains("恢复"));
        assert!(PathEvent::RolledBack {
            to: PathKind::Direct
        }
        .to_string()
        .contains("回滚"));
        assert!(Phase::Backoff { until_ms: 9_000 }
            .to_string()
            .contains("9000"));
        assert!(Action::None.is_none());
        assert!(!Action::SwitchTo(PathKind::Relay).is_none());
    }
}
