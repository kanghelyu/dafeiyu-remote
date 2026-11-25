// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Dafeiyu Remote contributors

//! # dfy-input — 键鼠注入、HID 映射与快捷键四态决策（大肥鱼远程）
//!
//! 本 crate 同时服务被控端与控制端：
//! - 被控端：[`InputSink`] + [`create_sink`] 把控制端送来的 [`InputEvent`] 注入系统
//!   （Windows 用 `SendInput`；mac/Linux 后端暂未实现，返回 [`InputError::Unsupported`]），
//!   并提供 `release_all`/`Drop` 防卡键兜底；
//! - 控制端：[`HidKeymap`] 负责 HID Usage ↔ VK ↔ 扫描码换算，
//!   [`decide_shortcut`] 负责快捷键四态决策（只发远端 / 只本地 / 两边 / 拦截）；
//! - 通用：[`utf16_units`] 处理 Unicode 文本拆分，[`testing::RecordingSink`] 供测试记录事件。
//!
//! 平台差异：只有 Windows 后端在本 crate 内实现；非 Windows 平台 `create_sink` 返回
//! 明确错误而不是 panic，保持接口形状不变。
//!
//! ```no_run
//! # fn demo() -> anyhow::Result<()> {
//! use dfy_input::{create_sink, InputEvent, KeyState};
//! let mut sink = create_sink()?;
//! sink.inject(&InputEvent::Key { usage: 0x04, state: KeyState::Down, modifiers: 0 })?;
//! sink.release_all();
//! # Ok(()) }
//! ```

#![deny(unsafe_op_in_unsafe_fn)]

pub mod event;
pub mod keymap;
pub mod shortcut;
pub mod testing;

mod normalize;
#[cfg(not(windows))]
mod unsupported;
#[cfg(windows)]
mod win;

pub use event::{
    utf16_units, InputError, InputEvent, InputSink, KeyState, MouseButton, PressedState,
};
pub use keymap::{HidEntry, HidKeymap, EXTENDED_PREFIX, PAUSE_SCANCODE};
pub use shortcut::{decide_shortcut, ShortcutContext, ShortcutDecision, ShortcutTable};

/// 创建当前平台的输入注入后端。
///
/// Windows：`SendInput` 实现；其他平台：返回 [`InputError::Unsupported`]。
#[cfg(windows)]
pub fn create_sink() -> anyhow::Result<Box<dyn InputSink>> {
    win::create_sink()
}

/// 创建当前平台的输入注入后端。
///
/// Windows：`SendInput` 实现；其他平台：返回 [`InputError::Unsupported`]。
#[cfg(not(windows))]
pub fn create_sink() -> anyhow::Result<Box<dyn InputSink>> {
    unsupported::create_sink()
}
