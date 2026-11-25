// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Dafeiyu Remote contributors

//! `dfy-path`：P2P 直连 ↔ 中继 的路径质量监测与切换决策（纯逻辑、无 IO、无时钟）。
//!
//! 背景见 `design/028-path-migration.md` 与 `notes/000-owner-brief.md` §17：
//! P2P 优先，质量不合格时切中继；切换通过「新 socket + 重新握手 + `SessionResume`
//! 续帧」实现，是可恢复的会话迁移而非断开重连（`design/027-session-resume.md`）。
//!
//! 本 crate 只做决策，不建连接、不发包、不读系统时钟：
//! - 采样入口：[`PathMonitor::on_sample`]（当前路径）与 [`PathMonitor::on_sample_for`]
//!   （另一条候选路径的探测采样）；
//! - 采样值：[`PathSample`]（RTT / 丢包 ppm / 有效接收速率 / stall / 毫秒时间戳）；
//! - 阈值：[`PathConfig`]（全部可覆盖，默认值见 [`PathConfig::default`] 与设计文档 §5）；
//! - 输出：[`Action`]（`SwitchTo` / `RequestProbe`）与 [`PathEvent`]（日志/指标钩子）；
//! - 统计：[`PathStats`]。
//!
//! 典型集成循环（建议 250ms 一拍，时间取调用方的单调时钟）：
//!
//! ```
//! use dfy_path::{Action, PathConfig, PathMonitor, PathSample};
//!
//! let mut monitor = PathMonitor::new(PathConfig::default());
//! for i in 0..10 {
//!     let sample = PathSample {
//!         rtt_ms: 20.0,
//!         loss_ppm: 0,
//!         recv_bps: 8_000_000,
//!         stall_ms: 0,
//!         at_ms: 1_000 + i * 250,
//!     };
//!     assert_eq!(monitor.on_sample(sample), Action::None);
//! }
//! // 前 5 个良好采样建立基线；平稳网络不触发切换。
//! assert!(monitor.state().baseline.is_some());
//! assert_eq!(monitor.stats().switches, 0);
//! ```
//!
//! # 确定性
//! 所有时间来自 `PathSample::at_ms`，没有 `Instant::now()`；同样的采样序列必然得到同样的
//! 动作与事件，便于无网络单测（见各模块 `#[cfg(test)]` 与 `tests/machine.rs`）。
//!
//! # 与传输/会话层的关系
//! `dfy-path` 不依赖 `dfy-transport` / `dfy-session`，避免循环依赖；调用方把
//! `dfy_session::ClientRunStats`（rtt、帧计数）与 `dfy_transport::Heartbeat` 的 RTT
//! 折算成 [`PathSample`] 喂进来即可。切换动作的具体执行（新建 QUIC 连接、握手、
//! `SessionResume`）由 CLI/session 层完成，完成后调用
//! [`PathMonitor::on_switch_committed`] 或 [`PathMonitor::on_switch_failed`] 回填结果。

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod config;
pub mod machine;
pub mod sample;
pub mod state;

pub use config::PathConfig;
pub use machine::PathMonitor;
pub use sample::{PathSample, SampleWindow, WindowStats};
pub use state::{
    Action, Baseline, DegradeReason, PathEvent, PathKind, PathState, PathStats, PendingSwitch,
    Phase, SwitchDirection,
};