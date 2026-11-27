// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! Windows 注入后端：基于 `SendInput` 的键盘/鼠标/文本注入。
//!
//! 设计要点：
//! - 键盘统一用 `KEYEVENTF_SCANCODE` 注入（按词法无关的扫描码，避免 IME/布局把
//!   字母变成别的字符）；扩展键（方向键、导航键、右 Ctrl/Alt/Win、小键盘 `/` 等）
//!   额外加 `KEYEVENTF_EXTENDEDKEY`；Pause 用 `VK_PAUSE` 让系统合成 E1 1D 45 序列。
//! - 文本用 `KEYEVENTF_UNICODE` 逐 UTF-16 码元注入，代理对自然拆分。
//! - 绝对鼠标用 `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`，把采集画面坐标
//!   归一化到整个虚拟桌面（`GetSystemMetrics(SM_*VIRTUALSCREEN)`）；这样多屏、
//!   虚拟显示器（如 UU/IDD 扩出来的屏）也在同一坐标系内正确落点。
//! - 一次 `InputEvent` 对应的多条底层事件（如文本、双轴滚轮）合并为一次
//!   `SendInput` 调用，减少注入延迟。
//! - [`InputSink::release_all`] 与 `Drop` 都会补发所有按下键/鼠标键的 Up 事件，
//!   防止会话异常断开后远端卡键。

#![allow(unsafe_code)]

use crate::event::{
    utf16_units, InputError, InputEvent, InputSink, KeyState, MouseButton, PressedState,
};
use crate::keymap::{HidKeymap, EXTENDED_PREFIX, PAUSE_SCANCODE};
use crate::normalize::normalize_abs;
use tracing::{debug, warn};
use windows::Win32::Foundation::GetLastError;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MapVirtualKeyW, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE,
    KEYEVENTF_UNICODE, MAPVK_VK_TO_VSC_EX, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
    MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, MOUSE_EVENT_FLAGS,
    VIRTUAL_KEY, VK_PAUSE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, XBUTTON1, XBUTTON2,
};

/// 虚拟桌面矩形（像素）。归一化使用 `VIRTUALDESK`，因此只需尺寸全部信息用于诊断/回退。
#[derive(Debug, Clone, Copy)]
struct DesktopRect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

impl DesktopRect {
    fn query() -> Self {
        // SAFETY: GetSystemMetrics 无参数/无副作用，返回当前会话的只读系统指标。
        unsafe {
            Self {
                x: GetSystemMetrics(SM_XVIRTUALSCREEN),
                y: GetSystemMetrics(SM_YVIRTUALSCREEN),
                width: GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1),
                height: GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1),
            }
        }
    }
}

/// Windows 注入后端。
pub(crate) struct WindowsSink {
    desktop: DesktopRect,
    pressed: PressedState,
}

impl WindowsSink {
    fn new() -> Self {
        let desktop = DesktopRect::query();
        // SAFETY: GetForegroundWindow 只读取当前前台窗口句柄，无参数、无副作用。
        let foreground = unsafe { GetForegroundWindow() };
        // MVP 只记录前台信息：若被注入目标是以管理员权限运行的进程，
        // 非提权进程的 SendInput 会被 UIPI 静默丢弃，届时 send_inputs 会带错误码返回。
        debug!(
            vx = desktop.x,
            vy = desktop.y,
            vw = desktop.width,
            vh = desktop.height,
            foreground_null = foreground.0.is_null(),
            "dfy-input Windows 注入后端就绪"
        );
        Self {
            desktop,
            pressed: PressedState::new(),
        }
    }

    /// 为一个输入事件构造底层 `INPUT` 数组（可能为空，例如未知键）。
    fn build_inputs(&self, ev: &InputEvent) -> anyhow::Result<Vec<INPUT>> {
        match ev {
            InputEvent::Key { usage, state, .. } => Ok(key_inputs(*usage, *state)),
            InputEvent::MouseButton { button, state } => {
                let down = *state == KeyState::Down;
                let (flags, data) = match button {
                    MouseButton::Left => (
                        if down {
                            MOUSEEVENTF_LEFTDOWN
                        } else {
                            MOUSEEVENTF_LEFTUP
                        },
                        0,
                    ),
                    MouseButton::Right => (
                        if down {
                            MOUSEEVENTF_RIGHTDOWN
                        } else {
                            MOUSEEVENTF_RIGHTUP
                        },
                        0,
                    ),
                    MouseButton::Middle => (
                        if down {
                            MOUSEEVENTF_MIDDLEDOWN
                        } else {
                            MOUSEEVENTF_MIDDLEUP
                        },
                        0,
                    ),
                    MouseButton::Back => (
                        if down {
                            MOUSEEVENTF_XDOWN
                        } else {
                            MOUSEEVENTF_XUP
                        },
                        u32::from(XBUTTON1),
                    ),
                    MouseButton::Forward => (
                        if down {
                            MOUSEEVENTF_XDOWN
                        } else {
                            MOUSEEVENTF_XUP
                        },
                        u32::from(XBUTTON2),
                    ),
                };
                Ok(vec![mouse_input(0, 0, data, flags)])
            }
            InputEvent::MouseMoveRel { dx, dy } => Ok(vec![mouse_input(
                i32::from(*dx),
                i32::from(*dy),
                0,
                MOUSEEVENTF_MOVE,
            )]),
            InputEvent::MouseMoveAbs {
                x,
                y,
                width,
                height,
            } => {
                let (w, h) = self.frame_extent(*width, *height);
                let (nx, ny) = normalize_abs(*x, *y, w, h);
                Ok(vec![mouse_input(
                    nx,
                    ny,
                    0,
                    MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                )])
            }
            InputEvent::Wheel { dx, dy } => {
                let mut inputs = Vec::with_capacity(2);
                if *dy != 0 {
                    inputs.push(mouse_input(0, 0, i32::from(*dy) as u32, MOUSEEVENTF_WHEEL));
                }
                if *dx != 0 {
                    inputs.push(mouse_input(0, 0, i32::from(*dx) as u32, MOUSEEVENTF_HWHEEL));
                }
                Ok(inputs)
            }
            InputEvent::Text { text } => {
                let units = utf16_units(text);
                let mut inputs = Vec::with_capacity(units.len() * 2);
                for unit in units {
                    inputs.push(keyboard_input(0, unit, KEYEVENTF_UNICODE));
                    inputs.push(keyboard_input(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
                }
                Ok(inputs)
            }
        }
    }

    /// 采集画面尺寸；缺失（0）时回退到整个虚拟桌面尺寸。
    fn frame_extent(&self, width: u32, height: u32) -> (u32, u32) {
        if width > 0 && height > 0 {
            return (width, height);
        }
        let w = self.desktop.width.max(1) as u32;
        let h = self.desktop.height.max(1) as u32;
        warn!(
            width,
            height, w, h, "绝对鼠标事件缺少画面尺寸，回退到虚拟桌面尺寸"
        );
        (w, h)
    }
}

impl InputSink for WindowsSink {
    fn inject(&mut self, ev: &InputEvent) -> anyhow::Result<()> {
        let inputs = self.build_inputs(ev)?;
        if inputs.is_empty() {
            // 未知 Usage 等情形：忽略但不记录按下状态，避免 release_all 再次失败。
            return Ok(());
        }
        send_inputs(&inputs)?;
        self.pressed.track(ev);
        Ok(())
    }

    fn release_all(&mut self) {
        let releases = self.pressed.release_events();
        if releases.is_empty() {
            return;
        }
        debug!(count = releases.len(), "补发按下状态释放事件");
        for ev in &releases {
            match self.build_inputs(ev) {
                Ok(inputs) if !inputs.is_empty() => {
                    if let Err(err) = send_inputs(&inputs) {
                        warn!(error = %err, "补发释放事件失败");
                    }
                }
                Ok(_) => {}
                Err(err) => warn!(error = %err, "构建释放事件失败"),
            }
        }
    }
}

impl Drop for WindowsSink {
    fn drop(&mut self) {
        // 对象销毁（会话结束/异常路径）时兜底防卡键。
        self.release_all();
    }
}

/// 创建 Windows 注入后端。
pub(crate) fn create_sink() -> anyhow::Result<Box<dyn InputSink>> {
    Ok(Box::new(WindowsSink::new()))
}

/// 把 HID Usage 转成 `INPUT` 序列。未知 Usage 返回空数组（调用方视为忽略）。
fn key_inputs(usage: u16, state: KeyState) -> Vec<INPUT> {
    let key_up = state == KeyState::Up;
    if let Some(scancode) = HidKeymap::usage_to_scancode(usage) {
        if scancode == PAUSE_SCANCODE {
            return vec![pause_input(key_up)];
        }
        return vec![scancode_input(scancode, key_up)];
    }
    // 回退路径：usage→VK→set-1 扫描码（MapVirtualKeyW 对未知 VK 返回 0）。
    if let Some(vk) = HidKeymap::usage_to_vk(usage) {
        if let Some(scancode) = vk_to_scancode(vk) {
            return vec![scancode_input(scancode, key_up)];
        }
    }
    warn!(usage, "未映射的 HID Usage，已忽略");
    Vec::new()
}

/// VK → set-1 扫描码；扩展键返回 `0xE0xx`。
fn vk_to_scancode(vk: u16) -> Option<u16> {
    // SAFETY: MapVirtualKeyW 是纯查询函数，输入为 VK 码，无副作用。
    let mapped = unsafe { MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_VSC_EX) };
    if mapped == 0 {
        None
    } else {
        Some((mapped & 0xFFFF) as u16)
    }
}

/// 由扫描码构造信号；扩展前缀拆成 `KEYEVENTF_EXTENDEDKEY`。
fn scancode_input(scancode: u16, key_up: bool) -> INPUT {
    let mut flags = KEYEVENTF_SCANCODE;
    if scancode & 0xFF00 == EXTENDED_PREFIX {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if key_up {
        flags |= KEYEVENTF_KEYUP;
    }
    keyboard_input(0, scancode & 0x00FF, flags)
}

/// Pause：真实 set-1 序列为 E1 1D 45，单条扫描码无法表达，改用 VK 让系统合成。
fn pause_input(key_up: bool) -> INPUT {
    let mut flags = KEYBD_EVENT_FLAGS::default();
    if key_up {
        flags |= KEYEVENTF_KEYUP;
    }
    keyboard_input(VK_PAUSE.0, 0, flags)
}

fn keyboard_input(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn mouse_input(dx: i32, dy: i32, mouse_data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: mouse_data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// 一次性批量注入；返回实际注入数量不符时带 `GetLastError` 报错。
fn send_inputs(inputs: &[INPUT]) -> anyhow::Result<()> {
    let total = inputs.len();
    // SAFETY: inputs 为有效 INPUT 数组，长度与 cbSize 匹配，调用期间保持有效。
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize == total {
        return Ok(());
    }
    // SAFETY: GetLastError 只读取当前线程的错误码。
    let last_error = unsafe { GetLastError() };
    Err(InputError::Send(format!(
        "SendInput 仅接受 {sent}/{total} 条事件，GetLastError={}（可能被 UIPI/前台进程权限拦截）",
        last_error.0
    ))
    .into())
}
