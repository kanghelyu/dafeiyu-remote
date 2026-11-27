// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! X11 的物理键名、坐标和状态机；不连接显示服务器，可在任何测试主机上验证。

use std::collections::HashMap;

use crate::{InputError, InputEvent, InputSink, KeyState, MouseButton};
use tracing::warn;

/// XKB 四字节物理键名。字母按物理位置映射，目标端的布局/IME 仍由目标系统处理。
pub(crate) fn physical_key_name(usage: u16) -> Option<[u8; 4]> {
    Some(*match usage {
        0x04 => b"AC01", // A
        0x05 => b"AB05", // B
        0x06 => b"AB03", // C
        0x07 => b"AC03", // D
        0x08 => b"AD03", // E
        0x09 => b"AC04", // F
        0x0a => b"AC05", // G
        0x0b => b"AC06", // H
        0x0c => b"AD08", // I
        0x0d => b"AC07", // J
        0x0e => b"AC08", // K
        0x0f => b"AC09", // L
        0x10 => b"AB07", // M
        0x11 => b"AB06", // N
        0x12 => b"AD09", // O
        0x13 => b"AD10", // P
        0x14 => b"AD01", // Q
        0x15 => b"AD04", // R
        0x16 => b"AC02", // S
        0x17 => b"AD05", // T
        0x18 => b"AD07", // U
        0x19 => b"AB04", // V
        0x1a => b"AD02", // W
        0x1b => b"AB02", // X
        0x1c => b"AD06", // Y
        0x1d => b"AB01", // Z
        0x1e => b"AE01",
        0x1f => b"AE02",
        0x20 => b"AE03",
        0x21 => b"AE04",
        0x22 => b"AE05",
        0x23 => b"AE06",
        0x24 => b"AE07",
        0x25 => b"AE08",
        0x26 => b"AE09",
        0x27 => b"AE10",
        0x28 => b"RTRN",
        0x29 => b"ESC\0",
        0x2a => b"BKSP",
        0x2b => b"TAB\0",
        0x2c => b"SPCE",
        0x2d => b"AE11",
        0x2e => b"AE12",
        0x2f => b"AD11",
        0x30 => b"AD12",
        0x31 | 0x32 => b"BKSL",
        0x33 => b"AC10",
        0x34 => b"AC11",
        0x35 => b"TLDE",
        0x36 => b"AB08",
        0x37 => b"AB09",
        0x38 => b"AB10",
        0x39 => b"CAPS",
        0x3a => b"FK01",
        0x3b => b"FK02",
        0x3c => b"FK03",
        0x3d => b"FK04",
        0x3e => b"FK05",
        0x3f => b"FK06",
        0x40 => b"FK07",
        0x41 => b"FK08",
        0x42 => b"FK09",
        0x43 => b"FK10",
        0x44 => b"FK11",
        0x45 => b"FK12",
        0x46 => b"PRSC",
        0x47 => b"SCLK",
        0x48 => b"PAUS",
        0x49 => b"INS\0",
        0x4a => b"HOME",
        0x4b => b"PGUP",
        0x4c => b"DELE",
        0x4d => b"END\0",
        0x4e => b"PGDN",
        0x4f => b"RGHT",
        0x50 => b"LEFT",
        0x51 => b"DOWN",
        0x52 => b"UP\0\0",
        0x53 => b"NMLK",
        0x54 => b"KPDV",
        0x55 => b"KPMU",
        0x56 => b"KPSU",
        0x57 => b"KPAD",
        0x58 => b"KPEN",
        0x59 => b"KP1\0",
        0x5a => b"KP2\0",
        0x5b => b"KP3\0",
        0x5c => b"KP4\0",
        0x5d => b"KP5\0",
        0x5e => b"KP6\0",
        0x5f => b"KP7\0",
        0x60 => b"KP8\0",
        0x61 => b"KP9\0",
        0x62 => b"KP0\0",
        0x63 => b"KPDL",
        0x64 => b"LSGT",
        0x65 => b"MENU",
        0x66 => b"POWR",
        0x67 => b"KPEQ",
        0x68 => b"FK13",
        0x69 => b"FK14",
        0x6a => b"FK15",
        0x6b => b"FK16",
        0x6c => b"FK17",
        0x6d => b"FK18",
        0x6e => b"FK19",
        0x6f => b"FK20",
        0x70 => b"FK21",
        0x71 => b"FK22",
        0x72 => b"FK23",
        0x73 => b"FK24",
        0x7f => b"MUTE",
        0x80 => b"VOL+",
        0x81 => b"VOL-",
        0x87 => b"AB11", // International1 (ろ)
        0x88 => b"HKTG", // International2 (ひらがな/カタカナ)
        0x89 => b"AE13", // International3 (¥)
        0x8a => b"HENK",
        0x8b => b"MUHE",
        0x90 => b"HNGL",
        0x91 => b"HJCV",
        0xe0 => b"LCTL",
        0xe1 => b"LFSH",
        0xe2 => b"LALT",
        0xe3 => b"LWIN",
        0xe4 => b"RCTL",
        0xe5 => b"RTSH",
        0xe6 => b"RALT",
        0xe7 => b"RWIN",
        _ => return None,
    })
}

pub(crate) struct PhysicalKeymap {
    names: HashMap<[u8; 4], u8>,
}

impl PhysicalKeymap {
    /// `first_key` 由 XKB GetNames 返回，绝不能假定为 evdev 键码 + 8。
    pub(crate) fn new(
        first_key: u8,
        names: &[[u8; 4]],
        aliases: &[([u8; 4], [u8; 4])],
    ) -> anyhow::Result<Self> {
        if first_key < 8 || names.is_empty() || usize::from(first_key) + names.len() > 256 {
            return Err(InputError::Unavailable("XKB 返回无效的物理键名范围".into()).into());
        }
        let mut map = HashMap::new();
        for (offset, name) in names.iter().enumerate() {
            if *name == [0; 4] {
                continue;
            }
            if map
                .insert(*name, (usize::from(first_key) + offset) as u8)
                .is_some()
            {
                return Err(InputError::Unavailable("XKB 返回重复的物理键名".into()).into());
            }
        }
        for (real, alias) in aliases {
            if let Some(code) = map.get(real).copied() {
                map.entry(*alias).or_insert(code);
            }
        }
        Ok(Self { names: map })
    }

    pub(crate) fn keycode(&self, usage: u16) -> anyhow::Result<u8> {
        let name = physical_key_name(usage).ok_or_else(|| {
            InputError::UnsupportedEvent(format!("未映射的 Keyboard/Keypad HID Usage {usage:#06x}"))
        })?;
        self.names.get(&name).copied().ok_or_else(|| {
            InputError::UnsupportedEvent(format!(
                "当前 XKB 键盘不含 HID Usage {usage:#06x} 对应的物理键 {}",
                String::from_utf8_lossy(&name).trim_end_matches('\0')
            ))
            .into()
        })
    }
}

/// 只接受明确的 X11 会话；不尝试通过 XWayland 越过 Wayland 授权边界。
pub(crate) fn require_x11_session(
    session_type: Option<&str>,
    wayland_display: Option<&str>,
    display: Option<&str>,
) -> anyhow::Result<()> {
    let explicit_x11 = session_type.is_some_and(|s| s.eq_ignore_ascii_case("x11"));
    if session_type.is_some_and(|s| s.eq_ignore_ascii_case("wayland"))
        || (!explicit_x11 && wayland_display.is_some_and(|s| !s.is_empty()))
    {
        return Err(InputError::UnsupportedSession(
            "Wayland 需要用户授权的 RemoteDesktop portal；X11/XTest 后端不向 Wayland 或 XWayland 注入，请登录 X11 会话".into(),
        ).into());
    }
    if matches!(display, None | Some("")) {
        return Err(InputError::Unavailable(
            "未设置 DISPLAY；需要可访问且经过 Xauthority 授权的 X11 显示会话".into(),
        )
        .into());
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum X11Event {
    Key { code: u8, state: KeyState },
    Button { button: u8, state: KeyState },
    Motion { relative: bool, x: i16, y: i16 },
}

pub(crate) trait X11Driver: Send {
    fn keycode(&mut self, usage: u16) -> anyhow::Result<u8>;
    fn geometry(&mut self) -> anyhow::Result<(u32, u32)>;
    /// 成功表示服务器已检查整批请求；失败可能已经发送部分请求。
    fn send(&mut self, events: &[X11Event]) -> anyhow::Result<()>;
}

/// 按住时保存实际键码；映射改变后 Up/release_all 仍释放原来的键。
pub(crate) struct X11Sink<D: X11Driver> {
    driver: D,
    keys: Vec<(u16, u8)>,
    buttons: Vec<u8>,
    wheel_x: i32,
    wheel_y: i32,
}

impl<D: X11Driver> X11Sink<D> {
    pub(crate) fn new(driver: D) -> Self {
        Self {
            driver,
            keys: Vec::new(),
            buttons: Vec::new(),
            wheel_x: 0,
            wheel_y: 0,
        }
    }

    fn key(&mut self, usage: u16, state: KeyState) -> anyhow::Result<()> {
        let held = self.keys.iter().find(|(hid, _)| *hid == usage).copied();
        let code = match held {
            Some((_, code)) => code,
            None => self.driver.keycode(usage)?,
        };
        let shared = self
            .keys
            .iter()
            .any(|(hid, keycode)| *hid != usage && *keycode == code);
        // 即使请求报错也可能已经被服务器执行，先保留释放兜底。
        if state == KeyState::Down && held.is_none() {
            self.keys.push((usage, code));
        }
        // HID Non-US # 与 Backslash 可能共用 BKSL；最后一个持有者才发 Up。
        if shared {
            if state == KeyState::Up {
                self.keys.retain(|(hid, _)| *hid != usage);
            }
            return Ok(());
        }
        self.driver.send(&[X11Event::Key { code, state }])?;
        if state == KeyState::Up {
            self.keys.retain(|(hid, _)| *hid != usage);
        }
        Ok(())
    }

    fn button(&mut self, button: u8, state: KeyState) -> anyhow::Result<()> {
        if state == KeyState::Down && !self.buttons.contains(&button) {
            self.buttons.push(button);
        }
        self.driver.send(&[X11Event::Button { button, state }])?;
        if state == KeyState::Up {
            self.buttons.retain(|b| *b != button);
        }
        Ok(())
    }

    fn wheel(&mut self, dx: i16, dy: i16) -> anyhow::Result<()> {
        self.wheel_x = self.wheel_x.saturating_add(i32::from(dx));
        self.wheel_y = self.wheel_y.saturating_add(i32::from(dy));
        // X11 core wheel 是 4/5（上/下）、6/7（左/右），保留不足 120 的增量。
        while self.wheel_y.unsigned_abs() >= 120 {
            let sign = self.wheel_y.signum();
            self.wheel_click(if sign > 0 { 4 } else { 5 })?;
            self.wheel_y -= sign * 120;
        }
        while self.wheel_x.unsigned_abs() >= 120 {
            let sign = self.wheel_x.signum();
            self.wheel_click(if sign > 0 { 7 } else { 6 })?;
            self.wheel_x -= sign * 120;
        }
        Ok(())
    }

    fn wheel_click(&mut self, button: u8) -> anyhow::Result<()> {
        if !self.buttons.contains(&button) {
            self.buttons.push(button);
        }
        self.driver.send(&[
            X11Event::Button {
                button,
                state: KeyState::Down,
            },
            X11Event::Button {
                button,
                state: KeyState::Up,
            },
        ])?;
        self.buttons.retain(|b| *b != button);
        Ok(())
    }
}

impl<D: X11Driver> InputSink for X11Sink<D> {
    fn inject(&mut self, ev: &InputEvent) -> anyhow::Result<()> {
        match ev {
            InputEvent::Key { usage, state, .. } => self.key(*usage, *state),
            InputEvent::MouseButton { button, state } => self.button(match button {
                MouseButton::Left => 1,
                MouseButton::Middle => 2,
                MouseButton::Right => 3,
                MouseButton::Back => 8,
                MouseButton::Forward => 9,
            }, *state),
            InputEvent::MouseMoveRel { dx, dy } => self.driver.send(&[X11Event::Motion {
                relative: true, x: *dx, y: *dy,
            }]),
            InputEvent::MouseMoveAbs { x, y, width, height } => {
                let (root_width, root_height) = self.driver.geometry()?;
                let (width, height) = if *width == 0 || *height == 0 {
                    (root_width, root_height)
                } else {
                    (*width, *height)
                };
                self.driver.send(&[X11Event::Motion {
                    relative: false,
                    x: scale_axis(*x, width, root_width)?,
                    y: scale_axis(*y, height, root_height)?,
                }])
            }
            InputEvent::Wheel { dx, dy } => self.wheel(*dx, *dy),
            InputEvent::Text { text } if text.is_empty() => Ok(()),
            InputEvent::Text { .. } => Err(InputError::UnsupportedEvent(
                "X11/XTest 无直接 Unicode/IME 文本提交 API；请发送物理键事件并使用目标端布局/IME，或使用已授权的剪贴板粘贴".into(),
            ).into()),
        }
    }

    fn release_all(&mut self) {
        // 只有已确认的 Up 才移出记录；失败时下次 release_all/Drop 仍能重试。
        let mut attempted_codes = Vec::new();
        for (usage, code) in self.keys.clone() {
            if attempted_codes.contains(&code) {
                continue;
            }
            attempted_codes.push(code);
            match self.driver.send(&[X11Event::Key {
                code,
                state: KeyState::Up,
            }]) {
                Ok(()) => self.keys.retain(|(_, keycode)| *keycode != code),
                Err(err) => warn!(usage, code, error = %err, "X11 释放按键失败，保留待重试状态"),
            }
        }
        for button in self.buttons.clone() {
            match self.driver.send(&[X11Event::Button {
                button,
                state: KeyState::Up,
            }]) {
                Ok(()) => self.buttons.retain(|b| *b != button),
                Err(err) => warn!(button, error = %err, "X11 释放鼠标键失败，保留待重试状态"),
            }
        }
        self.wheel_x = 0;
        self.wheel_y = 0;
    }
}

impl<D: X11Driver> Drop for X11Sink<D> {
    fn drop(&mut self) {
        self.release_all();
    }
}

#[cfg(test)]
mod tests;

fn scale_axis(value: u32, frame: u32, root: u32) -> anyhow::Result<i16> {
    if root == 0 || root > i16::MAX as u32 + 1 {
        return Err(InputError::UnsupportedEvent(format!(
            "XTest 无法表示尺寸为 {root} 的桌面坐标轴（范围 1..=32768）"
        ))
        .into());
    }
    if frame <= 1 || root == 1 {
        return Ok(0);
    }
    Ok((u64::from(value.min(frame - 1)) * u64::from(root - 1) / u64::from(frame - 1)) as i16)
}
