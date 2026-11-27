// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 键盘/鼠标按键 → HID Usage 映射。
//!
//! 与 `dfy-input`（`crates/dfy-input/src/keymap.rs`）以及 DFYP/1 协议保持一致：
//!
//! - 键盘使用 HID Keyboard/Keypad page（`0x07`）的 Usage ID，**不是** Windows VK，
//!   也不是扫描码。`KeyA = 0x04` … `KeyZ = 0x1D`，`Digit1 = 0x1E` … `Digit0 = 0x27`，
//!   `Enter = 0x28`、`Escape = 0x29`、`Backspace = 0x2A`、`Tab = 0x2B`、`Space = 0x2C`，
//!   方向键 `Right = 0x4F`、`Left = 0x50`、`Down = 0x51`、`Up = 0x52`，
//!   修饰键 `LCtrl = 0xE0` … `RGui = 0xE7`。
//! - 修饰键位图 bit0=LCtrl、bit1=LShift、bit2=LAlt、bit3=LGui、
//!   bit4=RCtrl、bit5=RShift、bit6=RAlt、bit7=RGui（左/右分开记）。
//! - 鼠标按键编号沿用协议里的 `hid_usage` 字段：1=左、2=右、3=中、4=后退、5=前进。
//!
//! 这里只做「物理键 → Usage」的翻译，不做布局（QWERTY/Dvorak）转换：
//! 远控要的是**物理位置**（按 A 就是 A 的位置），与 winit 的 `PhysicalKey` 语义一致。

use winit::event::MouseButton;
use winit::keyboard::KeyCode;

/// winit 物理按键 → HID Usage ID；不认识（或不需要转发）的键返回 `None`。
#[must_use]
pub fn hid_usage(code: KeyCode) -> Option<u16> {
    // 字母 A..Z：0x04..=0x1D
    let usage = match code {
        KeyCode::KeyA => 0x04,
        KeyCode::KeyB => 0x05,
        KeyCode::KeyC => 0x06,
        KeyCode::KeyD => 0x07,
        KeyCode::KeyE => 0x08,
        KeyCode::KeyF => 0x09,
        KeyCode::KeyG => 0x0A,
        KeyCode::KeyH => 0x0B,
        KeyCode::KeyI => 0x0C,
        KeyCode::KeyJ => 0x0D,
        KeyCode::KeyK => 0x0E,
        KeyCode::KeyL => 0x0F,
        KeyCode::KeyM => 0x10,
        KeyCode::KeyN => 0x11,
        KeyCode::KeyO => 0x12,
        KeyCode::KeyP => 0x13,
        KeyCode::KeyQ => 0x14,
        KeyCode::KeyR => 0x15,
        KeyCode::KeyS => 0x16,
        KeyCode::KeyT => 0x17,
        KeyCode::KeyU => 0x18,
        KeyCode::KeyV => 0x19,
        KeyCode::KeyW => 0x1A,
        KeyCode::KeyX => 0x1B,
        KeyCode::KeyY => 0x1C,
        KeyCode::KeyZ => 0x1D,
        // 数字行 1..9 -> 0x1E..0x26，0 -> 0x27（HID 顺序是 1..9,0）。
        KeyCode::Digit1 => 0x1E,
        KeyCode::Digit2 => 0x1F,
        KeyCode::Digit3 => 0x20,
        KeyCode::Digit4 => 0x21,
        KeyCode::Digit5 => 0x22,
        KeyCode::Digit6 => 0x23,
        KeyCode::Digit7 => 0x24,
        KeyCode::Digit8 => 0x25,
        KeyCode::Digit9 => 0x26,
        KeyCode::Digit0 => 0x27,
        // 编辑/控制键
        KeyCode::Enter => 0x28,
        KeyCode::Escape => 0x29,
        KeyCode::Backspace => 0x2A,
        KeyCode::Tab => 0x2B,
        KeyCode::Space => 0x2C,
        KeyCode::Minus => 0x2D,
        KeyCode::Equal => 0x2E,
        KeyCode::BracketLeft => 0x2F,
        KeyCode::BracketRight => 0x30,
        KeyCode::Backslash => 0x31,
        KeyCode::Semicolon => 0x33,
        KeyCode::Quote => 0x34,
        KeyCode::Backquote => 0x35,
        KeyCode::Comma => 0x36,
        KeyCode::Period => 0x37,
        KeyCode::Slash => 0x38,
        KeyCode::CapsLock => 0x39,
        // 功能键 F1..F12 -> 0x3A..0x45；F13..F24 -> 0x68..0x73。
        KeyCode::F1 => 0x3A,
        KeyCode::F2 => 0x3B,
        KeyCode::F3 => 0x3C,
        KeyCode::F4 => 0x3D,
        KeyCode::F5 => 0x3E,
        KeyCode::F6 => 0x3F,
        KeyCode::F7 => 0x40,
        KeyCode::F8 => 0x41,
        KeyCode::F9 => 0x42,
        KeyCode::F10 => 0x43,
        KeyCode::F11 => 0x44,
        KeyCode::F12 => 0x45,
        KeyCode::F13 => 0x68,
        KeyCode::F14 => 0x69,
        KeyCode::F15 => 0x6A,
        KeyCode::F16 => 0x6B,
        KeyCode::F17 => 0x6C,
        KeyCode::F18 => 0x6D,
        KeyCode::F19 => 0x6E,
        KeyCode::F20 => 0x6F,
        KeyCode::F21 => 0x70,
        KeyCode::F22 => 0x71,
        KeyCode::F23 => 0x72,
        KeyCode::F24 => 0x73,
        // 系统键与导航键
        KeyCode::PrintScreen => 0x46,
        KeyCode::ScrollLock => 0x47,
        KeyCode::Pause => 0x48,
        KeyCode::Insert => 0x49,
        KeyCode::Home => 0x4A,
        KeyCode::PageUp => 0x4B,
        KeyCode::Delete => 0x4C,
        KeyCode::End => 0x4D,
        KeyCode::PageDown => 0x4E,
        KeyCode::ArrowRight => 0x4F,
        KeyCode::ArrowLeft => 0x50,
        KeyCode::ArrowDown => 0x51,
        KeyCode::ArrowUp => 0x52,
        // 小键盘
        KeyCode::NumLock => 0x53,
        KeyCode::NumpadDivide => 0x54,
        KeyCode::NumpadMultiply => 0x55,
        KeyCode::NumpadSubtract => 0x56,
        KeyCode::NumpadAdd => 0x57,
        KeyCode::NumpadEnter => 0x58,
        KeyCode::Numpad1 => 0x59,
        KeyCode::Numpad2 => 0x5A,
        KeyCode::Numpad3 => 0x5B,
        KeyCode::Numpad4 => 0x5C,
        KeyCode::Numpad5 => 0x5D,
        KeyCode::Numpad6 => 0x5E,
        KeyCode::Numpad7 => 0x5F,
        KeyCode::Numpad8 => 0x60,
        KeyCode::Numpad9 => 0x61,
        KeyCode::Numpad0 => 0x62,
        KeyCode::NumpadDecimal => 0x63,
        KeyCode::NumpadEqual => 0x67,
        KeyCode::ContextMenu => 0x65,
        // 修饰键（左/右分开）
        KeyCode::ControlLeft => 0xE0,
        KeyCode::ShiftLeft => 0xE1,
        KeyCode::AltLeft => 0xE2,
        KeyCode::SuperLeft => 0xE3,
        KeyCode::ControlRight => 0xE4,
        KeyCode::ShiftRight => 0xE5,
        KeyCode::AltRight => 0xE6,
        KeyCode::SuperRight => 0xE7,
        _ => return None,
    };
    Some(usage)
}

/// 修饰键在 modifiers 位图里的位号（bit0=LCtrl … bit7=RGui）；非修饰键返回 `None`。
#[must_use]
pub fn modifier_bit(code: KeyCode) -> Option<u8> {
    let bit = match code {
        KeyCode::ControlLeft => 0,
        KeyCode::ShiftLeft => 1,
        KeyCode::AltLeft => 2,
        KeyCode::SuperLeft => 3,
        KeyCode::ControlRight => 4,
        KeyCode::ShiftRight => 5,
        KeyCode::AltRight => 6,
        KeyCode::SuperRight => 7,
        _ => return None,
    };
    Some(bit)
}

/// winit 鼠标按键 → 协议里的鼠标 `hid_usage`（1=左、2=右、3=中、4=后退、5=前进）。
///
/// 侧键之外的 `Other(..)` 一律忽略：`dfy-input` 的 `MouseButton` 只有 5 个值。
#[must_use]
pub fn hid_button(button: MouseButton) -> Option<u8> {
    match button {
        MouseButton::Left => Some(1),
        MouseButton::Right => Some(2),
        MouseButton::Middle => Some(3),
        MouseButton::Back => Some(4),
        MouseButton::Forward => Some(5),
        MouseButton::Other(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_and_digits_match_dfy_input_table() {
        assert_eq!(hid_usage(KeyCode::KeyA), Some(0x04));
        assert_eq!(hid_usage(KeyCode::KeyZ), Some(0x1D));
        assert_eq!(hid_usage(KeyCode::KeyQ), Some(0x14));
        assert_eq!(hid_usage(KeyCode::Digit1), Some(0x1E));
        assert_eq!(hid_usage(KeyCode::Digit9), Some(0x26));
        assert_eq!(hid_usage(KeyCode::Digit0), Some(0x27));
    }

    #[test]
    fn editing_and_navigation_keys() {
        assert_eq!(hid_usage(KeyCode::Enter), Some(0x28));
        assert_eq!(hid_usage(KeyCode::Escape), Some(0x29));
        assert_eq!(hid_usage(KeyCode::Backspace), Some(0x2A));
        assert_eq!(hid_usage(KeyCode::Tab), Some(0x2B));
        assert_eq!(hid_usage(KeyCode::Space), Some(0x2C));
        assert_eq!(hid_usage(KeyCode::Delete), Some(0x4C));
        assert_eq!(hid_usage(KeyCode::ArrowRight), Some(0x4F));
        assert_eq!(hid_usage(KeyCode::ArrowLeft), Some(0x50));
        assert_eq!(hid_usage(KeyCode::ArrowDown), Some(0x51));
        assert_eq!(hid_usage(KeyCode::ArrowUp), Some(0x52));
    }

    #[test]
    fn function_keys_cover_f1_to_f24() {
        assert_eq!(hid_usage(KeyCode::F1), Some(0x3A));
        assert_eq!(hid_usage(KeyCode::F11), Some(0x44));
        assert_eq!(hid_usage(KeyCode::F12), Some(0x45));
        assert_eq!(hid_usage(KeyCode::F13), Some(0x68));
        assert_eq!(hid_usage(KeyCode::F24), Some(0x73));
    }

    #[test]
    fn modifiers_have_expected_usage_and_bit() {
        // 位图 bit 与 Usage 的偏移一致：bit0=0xE0 … bit7=0xE7。
        for bit in 0u8..8 {
            let code = match bit {
                0 => KeyCode::ControlLeft,
                1 => KeyCode::ShiftLeft,
                2 => KeyCode::AltLeft,
                3 => KeyCode::SuperLeft,
                4 => KeyCode::ControlRight,
                5 => KeyCode::ShiftRight,
                6 => KeyCode::AltRight,
                _ => KeyCode::SuperRight,
            };
            assert_eq!(modifier_bit(code), Some(bit));
            assert_eq!(hid_usage(code), Some(0xE0 + u16::from(bit)));
        }
        assert_eq!(modifier_bit(KeyCode::KeyA), None);
        assert_eq!(modifier_bit(KeyCode::F11), None);
    }

    #[test]
    fn mouse_buttons_map_to_protocol_ids() {
        assert_eq!(hid_button(MouseButton::Left), Some(1));
        assert_eq!(hid_button(MouseButton::Right), Some(2));
        assert_eq!(hid_button(MouseButton::Middle), Some(3));
        assert_eq!(hid_button(MouseButton::Back), Some(4));
        assert_eq!(hid_button(MouseButton::Forward), Some(5));
        assert_eq!(hid_button(MouseButton::Other(8)), None);
    }

    #[test]
    fn unknown_keys_are_ignored_not_misreported() {
        // 不转发的键必须返回 None，绝不能误映射成别的 Usage。
        assert_eq!(hid_usage(KeyCode::Help), None);
        assert_eq!(hid_usage(KeyCode::Fn), None);
        assert_eq!(hid_usage(KeyCode::FnLock), None);
    }
}
