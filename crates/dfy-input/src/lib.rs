// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! # dfy-input — 键鼠注入、HID 映射与快捷键四态决策（大肥鱼远程）
//!
//! 本 crate 同时服务被控端与控制端：
//! - 被控端：[`InputSink`] + [`create_sink`] 把 [`InputEvent`] 注入系统；Windows 使用
//!   `SendInput`，Linux X11 使用 XTest + XKB 物理键名，macOS 使用 `CGEvent`，
//!   并提供 `release_all`/`Drop` 防卡键；
//! - 控制端：[`HidKeymap`] 负责 HID Usage ↔ VK ↔ 扫描码换算，
//!   [`decide_shortcut`] 负责快捷键四态决策（只发远端 / 只本地 / 两边 / 拦截）；
//! - 通用：[`utf16_units`] 处理 Unicode 文本拆分，[`testing::RecordingSink`] 供测试记录事件。
//!
//! Linux 需要当前进程具有 `DISPLAY` 和 Xauthority 连接授权，且显示服务器提供 XTest
//! 与 XKB。Wayland 需要另行实现用户授权的 RemoteDesktop portal，本后端明确拒绝。
//! Linux 的物理键事件使用目标端布局/IME；直接 [`InputEvent::Text`] 返回明确错误。
//! XKB 物理键码表在建立 sink 时读取；重新配置物理键码后应重建 sink。
//! macOS 需要「辅助功能（Accessibility）」权限（`AXIsProcessTrusted`）；其它未支持平台
//! 返回 [`InputError::Unsupported`]。
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
pub mod macos_keymap;
pub mod shortcut;
pub mod testing;

#[cfg(target_os = "macos")]
mod macos;
mod normalize;
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod unsupported;
#[cfg(windows)]
mod win;
#[cfg(target_os = "linux")]
mod x11;
#[cfg(target_os = "linux")]
mod x11_common;

/// XKB physical key name for a HID Keyboard/Keypad usage.
///
/// This is primarily useful to controllers that observe X11 physical keycodes and need to send
/// layout-independent HID usages to a host with a different keyboard layout.
#[cfg(target_os = "linux")]
pub fn x11_usage_to_physical_key_name(usage: u16) -> Option<[u8; 4]> {
    x11_common::physical_key_name(usage)
}

/// Best-effort inverse of [`x11_usage_to_physical_key_name`].
///
/// XKB cannot distinguish every HID usage that shares a physical name (for example the two
/// backslash usages). In that case the later/canonical usage is selected consistently.
#[cfg(target_os = "linux")]
pub fn x11_physical_key_name_to_usage(name: [u8; 4]) -> Option<u16> {
    (0..=0xff)
        .rev()
        .find(|usage| x11_common::physical_key_name(*usage) == Some(name))
}

#[cfg(all(test, target_os = "linux"))]
mod x11_mapping_tests {
    use super::*;

    #[test]
    fn physical_key_names_roundtrip_for_unambiguous_usages() {
        for usage in [0x04, 0x14, 0x28, 0x68, 0xe0, 0xe7] {
            let name = x11_usage_to_physical_key_name(usage).expect("known usage");
            assert_eq!(x11_physical_key_name_to_usage(name), Some(usage));
        }
        assert_eq!(x11_usage_to_physical_key_name(0xffff), None);
        assert_eq!(x11_physical_key_name_to_usage(*b"NONE"), None);
    }

    #[test]
    fn shared_backslash_name_selects_canonical_usage() {
        assert_eq!(x11_physical_key_name_to_usage(*b"BKSL"), Some(0x32));
    }
}

pub use event::{
    utf16_units, InputError, InputEvent, InputSink, KeyState, MouseButton, PressedState,
};
pub use keymap::{HidEntry, HidKeymap, EXTENDED_PREFIX, PAUSE_SCANCODE};
pub use macos_keymap::{macos_keycode_to_usage, macos_usage_to_keycode};
pub use shortcut::{decide_shortcut, ShortcutContext, ShortcutDecision, ShortcutTable};

/// 创建 Windows `SendInput` 输入注入后端。
#[cfg(windows)]
pub fn create_sink() -> anyhow::Result<Box<dyn InputSink>> {
    win::create_sink()
}

/// 创建 Linux X11/XTest 输入注入后端。
///
/// Wayland、缺少 DISPLAY/Xauthority 授权、XTest/XKB 不可用时返回明确错误。
#[cfg(target_os = "linux")]
pub fn create_sink() -> anyhow::Result<Box<dyn InputSink>> {
    x11::create_sink()
}

/// 创建 macOS `CGEvent` 输入注入后端。
///
/// 当前进程未获「辅助功能（Accessibility）」授权时返回明确错误。
#[cfg(target_os = "macos")]
pub fn create_sink() -> anyhow::Result<Box<dyn InputSink>> {
    macos::create_sink()
}

/// Creates an X11 input sink on an explicitly selected managed display.
///
/// Unlike [`create_sink`], this does not require the process-wide desktop environment and
/// never changes `DISPLAY`. XWayland remains unsupported.
#[cfg(target_os = "linux")]
pub fn create_sink_on_display(display: &str) -> anyhow::Result<Box<dyn InputSink>> {
    x11::create_sink_on_display(display)
}

/// 其他平台返回 [`InputError::Unsupported`]。
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub fn create_sink() -> anyhow::Result<Box<dyn InputSink>> {
    unsupported::create_sink()
}
