// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Dafeiyu Remote contributors

//! HID Usage ↔ Windows 虚拟键（VK）↔ set-1 扫描码映射表。
//!
//! 协议层只传 HID Usage ID（Keyboard/Keypad page `0x07`），被控端在这里换算成
//! Windows 注入所需的值。扫描码编码约定：
//! - `0x0000..=0x00FF`：set-1 单字节扫描码；
//! - `0xE0xx`：E0 前缀的扩展键（注入时需 `KEYEVENTF_EXTENDEDKEY`）；
//! - `0xE11D`：Pause 专用标记（真实序列为 E1 1D 45，无法用单个 `u16` 表达，
//!   注入端改用 `VK_PAUSE` 让系统合成完整序列）。
//!
//! 表项覆盖字母 A–Z、主键区数字 1–0、Esc/Tab/CapsLock/Shift/Ctrl/Alt/GUI、
//! Space/Enter/Backspace、方向键、Insert/Delete/Home/End/PageUp/PageDown、
//! F1–F24、主键区符号、小键盘、PrintScreen/ScrollLock/Pause、ISO 额外键
//! （`0x64` Non-US `\|`）与 Application 键。

/// 一条映射表项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HidEntry {
    /// HID Usage ID（page `0x07`）。
    pub usage: u16,
    /// Windows 虚拟键码。
    pub vk: u16,
    /// set-1 扫描码（扩展键以 `0xE0xx` 编码，Pause 为 `0xE11D`）。
    pub scancode: u16,
}

/// Pause 的特殊扫描码标记，见模块文档。
pub const PAUSE_SCANCODE: u16 = 0xE11D;

/// 扩展键扫描码前缀（高字节 `0xE0`），用于 `scancode & 0xFF00` 判断。
pub const EXTENDED_PREFIX: u16 = 0xE000;

// 说明：usage 升序排列，便于人工核对；查表为线性扫描（键盘事件频率下开销可忽略）。
const TABLE: &[HidEntry] = &[
    // 字母 A–Z
    HidEntry {
        usage: 0x04,
        vk: 0x41,
        scancode: 0x1E,
    }, // A
    HidEntry {
        usage: 0x05,
        vk: 0x42,
        scancode: 0x30,
    }, // B
    HidEntry {
        usage: 0x06,
        vk: 0x43,
        scancode: 0x2E,
    }, // C
    HidEntry {
        usage: 0x07,
        vk: 0x44,
        scancode: 0x20,
    }, // D
    HidEntry {
        usage: 0x08,
        vk: 0x45,
        scancode: 0x12,
    }, // E
    HidEntry {
        usage: 0x09,
        vk: 0x46,
        scancode: 0x21,
    }, // F
    HidEntry {
        usage: 0x0A,
        vk: 0x47,
        scancode: 0x22,
    }, // G
    HidEntry {
        usage: 0x0B,
        vk: 0x48,
        scancode: 0x23,
    }, // H
    HidEntry {
        usage: 0x0C,
        vk: 0x49,
        scancode: 0x17,
    }, // I
    HidEntry {
        usage: 0x0D,
        vk: 0x4A,
        scancode: 0x24,
    }, // J
    HidEntry {
        usage: 0x0E,
        vk: 0x4B,
        scancode: 0x25,
    }, // K
    HidEntry {
        usage: 0x0F,
        vk: 0x4C,
        scancode: 0x26,
    }, // L
    HidEntry {
        usage: 0x10,
        vk: 0x4D,
        scancode: 0x32,
    }, // M
    HidEntry {
        usage: 0x11,
        vk: 0x4E,
        scancode: 0x31,
    }, // N
    HidEntry {
        usage: 0x12,
        vk: 0x4F,
        scancode: 0x18,
    }, // O
    HidEntry {
        usage: 0x13,
        vk: 0x50,
        scancode: 0x19,
    }, // P
    HidEntry {
        usage: 0x14,
        vk: 0x51,
        scancode: 0x10,
    }, // Q
    HidEntry {
        usage: 0x15,
        vk: 0x52,
        scancode: 0x13,
    }, // R
    HidEntry {
        usage: 0x16,
        vk: 0x53,
        scancode: 0x1F,
    }, // S
    HidEntry {
        usage: 0x17,
        vk: 0x54,
        scancode: 0x14,
    }, // T
    HidEntry {
        usage: 0x18,
        vk: 0x55,
        scancode: 0x16,
    }, // U
    HidEntry {
        usage: 0x19,
        vk: 0x56,
        scancode: 0x2F,
    }, // V
    HidEntry {
        usage: 0x1A,
        vk: 0x57,
        scancode: 0x11,
    }, // W
    HidEntry {
        usage: 0x1B,
        vk: 0x58,
        scancode: 0x2D,
    }, // X
    HidEntry {
        usage: 0x1C,
        vk: 0x59,
        scancode: 0x15,
    }, // Y
    HidEntry {
        usage: 0x1D,
        vk: 0x5A,
        scancode: 0x2C,
    }, // Z
    // 主键区数字 1–0
    HidEntry {
        usage: 0x1E,
        vk: 0x31,
        scancode: 0x02,
    }, // 1
    HidEntry {
        usage: 0x1F,
        vk: 0x32,
        scancode: 0x03,
    }, // 2
    HidEntry {
        usage: 0x20,
        vk: 0x33,
        scancode: 0x04,
    }, // 3
    HidEntry {
        usage: 0x21,
        vk: 0x34,
        scancode: 0x05,
    }, // 4
    HidEntry {
        usage: 0x22,
        vk: 0x35,
        scancode: 0x06,
    }, // 5
    HidEntry {
        usage: 0x23,
        vk: 0x36,
        scancode: 0x07,
    }, // 6
    HidEntry {
        usage: 0x24,
        vk: 0x37,
        scancode: 0x08,
    }, // 7
    HidEntry {
        usage: 0x25,
        vk: 0x38,
        scancode: 0x09,
    }, // 8
    HidEntry {
        usage: 0x26,
        vk: 0x39,
        scancode: 0x0A,
    }, // 9
    HidEntry {
        usage: 0x27,
        vk: 0x30,
        scancode: 0x0B,
    }, // 0
    // 控制键
    HidEntry {
        usage: 0x28,
        vk: 0x0D,
        scancode: 0x1C,
    }, // Enter
    HidEntry {
        usage: 0x29,
        vk: 0x1B,
        scancode: 0x01,
    }, // Esc
    HidEntry {
        usage: 0x2A,
        vk: 0x08,
        scancode: 0x0E,
    }, // Backspace
    HidEntry {
        usage: 0x2B,
        vk: 0x09,
        scancode: 0x0F,
    }, // Tab
    HidEntry {
        usage: 0x2C,
        vk: 0x20,
        scancode: 0x39,
    }, // Space
    HidEntry {
        usage: 0x2D,
        vk: 0xBD,
        scancode: 0x0C,
    }, // - _
    HidEntry {
        usage: 0x2E,
        vk: 0xBB,
        scancode: 0x0D,
    }, // = +
    HidEntry {
        usage: 0x2F,
        vk: 0xDB,
        scancode: 0x1A,
    }, // [ {
    HidEntry {
        usage: 0x30,
        vk: 0xDD,
        scancode: 0x1B,
    }, // ] }
    HidEntry {
        usage: 0x31,
        vk: 0xDC,
        scancode: 0x2B,
    }, // \ |
    HidEntry {
        usage: 0x33,
        vk: 0xBA,
        scancode: 0x27,
    }, // ; :
    HidEntry {
        usage: 0x34,
        vk: 0xDE,
        scancode: 0x28,
    }, // ' "
    HidEntry {
        usage: 0x35,
        vk: 0xC0,
        scancode: 0x29,
    }, // ` ~
    HidEntry {
        usage: 0x36,
        vk: 0xBC,
        scancode: 0x33,
    }, // , <
    HidEntry {
        usage: 0x37,
        vk: 0xBE,
        scancode: 0x34,
    }, // . >
    HidEntry {
        usage: 0x38,
        vk: 0xBF,
        scancode: 0x35,
    }, // / ?
    HidEntry {
        usage: 0x39,
        vk: 0x14,
        scancode: 0x3A,
    }, // CapsLock
    // 功能键 F1–F12
    HidEntry {
        usage: 0x3A,
        vk: 0x70,
        scancode: 0x3B,
    }, // F1
    HidEntry {
        usage: 0x3B,
        vk: 0x71,
        scancode: 0x3C,
    }, // F2
    HidEntry {
        usage: 0x3C,
        vk: 0x72,
        scancode: 0x3D,
    }, // F3
    HidEntry {
        usage: 0x3D,
        vk: 0x73,
        scancode: 0x3E,
    }, // F4
    HidEntry {
        usage: 0x3E,
        vk: 0x74,
        scancode: 0x3F,
    }, // F5
    HidEntry {
        usage: 0x3F,
        vk: 0x75,
        scancode: 0x40,
    }, // F6
    HidEntry {
        usage: 0x40,
        vk: 0x76,
        scancode: 0x41,
    }, // F7
    HidEntry {
        usage: 0x41,
        vk: 0x77,
        scancode: 0x42,
    }, // F8
    HidEntry {
        usage: 0x42,
        vk: 0x78,
        scancode: 0x43,
    }, // F9
    HidEntry {
        usage: 0x43,
        vk: 0x79,
        scancode: 0x44,
    }, // F10
    HidEntry {
        usage: 0x44,
        vk: 0x7A,
        scancode: 0x57,
    }, // F11
    HidEntry {
        usage: 0x45,
        vk: 0x7B,
        scancode: 0x58,
    }, // F12
    // 系统与编辑键
    HidEntry {
        usage: 0x46,
        vk: 0x2C,
        scancode: 0xE037,
    }, // PrintScreen
    HidEntry {
        usage: 0x47,
        vk: 0x91,
        scancode: 0x46,
    }, // ScrollLock
    HidEntry {
        usage: 0x48,
        vk: 0x13,
        scancode: PAUSE_SCANCODE,
    }, // Pause
    HidEntry {
        usage: 0x49,
        vk: 0x2D,
        scancode: 0xE052,
    }, // Insert
    HidEntry {
        usage: 0x4A,
        vk: 0x24,
        scancode: 0xE047,
    }, // Home
    HidEntry {
        usage: 0x4B,
        vk: 0x21,
        scancode: 0xE049,
    }, // PageUp
    HidEntry {
        usage: 0x4C,
        vk: 0x2E,
        scancode: 0xE053,
    }, // Delete
    HidEntry {
        usage: 0x4D,
        vk: 0x23,
        scancode: 0xE04F,
    }, // End
    HidEntry {
        usage: 0x4E,
        vk: 0x22,
        scancode: 0xE051,
    }, // PageDown
    HidEntry {
        usage: 0x4F,
        vk: 0x27,
        scancode: 0xE04D,
    }, // Right
    HidEntry {
        usage: 0x50,
        vk: 0x25,
        scancode: 0xE04B,
    }, // Left
    HidEntry {
        usage: 0x51,
        vk: 0x28,
        scancode: 0xE050,
    }, // Down
    HidEntry {
        usage: 0x52,
        vk: 0x26,
        scancode: 0xE048,
    }, // Up
    // 小键盘
    HidEntry {
        usage: 0x53,
        vk: 0x90,
        scancode: 0x45,
    }, // NumLock
    HidEntry {
        usage: 0x54,
        vk: 0x6F,
        scancode: 0xE035,
    }, // KP /
    HidEntry {
        usage: 0x55,
        vk: 0x6A,
        scancode: 0x37,
    }, // KP *
    HidEntry {
        usage: 0x56,
        vk: 0x6D,
        scancode: 0x4A,
    }, // KP -
    HidEntry {
        usage: 0x57,
        vk: 0x6B,
        scancode: 0x4E,
    }, // KP +
    HidEntry {
        usage: 0x58,
        vk: 0x0D,
        scancode: 0xE01C,
    }, // KP Enter
    HidEntry {
        usage: 0x59,
        vk: 0x61,
        scancode: 0x4F,
    }, // KP 1
    HidEntry {
        usage: 0x5A,
        vk: 0x62,
        scancode: 0x50,
    }, // KP 2
    HidEntry {
        usage: 0x5B,
        vk: 0x63,
        scancode: 0x51,
    }, // KP 3
    HidEntry {
        usage: 0x5C,
        vk: 0x64,
        scancode: 0x4B,
    }, // KP 4
    HidEntry {
        usage: 0x5D,
        vk: 0x65,
        scancode: 0x4C,
    }, // KP 5
    HidEntry {
        usage: 0x5E,
        vk: 0x66,
        scancode: 0x4D,
    }, // KP 6
    HidEntry {
        usage: 0x5F,
        vk: 0x67,
        scancode: 0x47,
    }, // KP 7
    HidEntry {
        usage: 0x60,
        vk: 0x68,
        scancode: 0x48,
    }, // KP 8
    HidEntry {
        usage: 0x61,
        vk: 0x69,
        scancode: 0x49,
    }, // KP 9
    HidEntry {
        usage: 0x62,
        vk: 0x60,
        scancode: 0x52,
    }, // KP 0
    HidEntry {
        usage: 0x63,
        vk: 0x6E,
        scancode: 0x53,
    }, // KP .
    // ISO 额外键与 Windows 扩展键
    HidEntry {
        usage: 0x64,
        vk: 0xE2,
        scancode: 0x56,
    }, // Non-US \ | (OEM_102)
    HidEntry {
        usage: 0x65,
        vk: 0x5D,
        scancode: 0xE05D,
    }, // Application / Menu
    HidEntry {
        usage: 0x67,
        vk: 0x92,
        scancode: 0x59,
    }, // KP =
    // F13–F24
    HidEntry {
        usage: 0x68,
        vk: 0x7C,
        scancode: 0x64,
    }, // F13
    HidEntry {
        usage: 0x69,
        vk: 0x7D,
        scancode: 0x65,
    }, // F14
    HidEntry {
        usage: 0x6A,
        vk: 0x7E,
        scancode: 0x66,
    }, // F15
    HidEntry {
        usage: 0x6B,
        vk: 0x7F,
        scancode: 0x67,
    }, // F16
    HidEntry {
        usage: 0x6C,
        vk: 0x80,
        scancode: 0x68,
    }, // F17
    HidEntry {
        usage: 0x6D,
        vk: 0x81,
        scancode: 0x69,
    }, // F18
    HidEntry {
        usage: 0x6E,
        vk: 0x82,
        scancode: 0x6A,
    }, // F19
    HidEntry {
        usage: 0x6F,
        vk: 0x83,
        scancode: 0x6B,
    }, // F20
    HidEntry {
        usage: 0x70,
        vk: 0x84,
        scancode: 0x6C,
    }, // F21
    HidEntry {
        usage: 0x71,
        vk: 0x85,
        scancode: 0x6D,
    }, // F22
    HidEntry {
        usage: 0x72,
        vk: 0x86,
        scancode: 0x6E,
    }, // F23
    HidEntry {
        usage: 0x73,
        vk: 0x87,
        scancode: 0x76,
    }, // F24
    // 修饰键（HID 0xE0–0xE7）
    HidEntry {
        usage: 0xE0,
        vk: 0xA2,
        scancode: 0x1D,
    }, // LeftCtrl
    HidEntry {
        usage: 0xE1,
        vk: 0xA0,
        scancode: 0x2A,
    }, // LeftShift
    HidEntry {
        usage: 0xE2,
        vk: 0xA4,
        scancode: 0x38,
    }, // LeftAlt
    HidEntry {
        usage: 0xE3,
        vk: 0x5B,
        scancode: 0xE05B,
    }, // LeftGUI
    HidEntry {
        usage: 0xE4,
        vk: 0xA3,
        scancode: 0xE01D,
    }, // RightCtrl
    HidEntry {
        usage: 0xE5,
        vk: 0xA1,
        scancode: 0x36,
    }, // RightShift
    HidEntry {
        usage: 0xE6,
        vk: 0xA5,
        scancode: 0xE038,
    }, // RightAlt
    HidEntry {
        usage: 0xE7,
        vk: 0x5C,
        scancode: 0xE05C,
    }, // RightGUI
];

/// 无状态的 HID 映射查询器。
#[derive(Debug, Clone, Copy, Default)]
pub struct HidKeymap;

impl HidKeymap {
    /// 全部映射表项（按 usage 升序），供诊断工具/示例打印与一致性测试遍历。
    #[must_use]
    pub fn entries() -> &'static [HidEntry] {
        TABLE
    }

    /// HID Usage → Windows 虚拟键码。
    #[must_use]
    pub fn usage_to_vk(usage: u16) -> Option<u16> {
        TABLE
            .iter()
            .find(|entry| entry.usage == usage)
            .map(|entry| entry.vk)
    }

    /// HID Usage → set-1 扫描码（扩展键为 `0xE0xx`，Pause 为 `0xE11D`）。
    #[must_use]
    pub fn usage_to_scancode(usage: u16) -> Option<u16> {
        TABLE
            .iter()
            .find(|entry| entry.usage == usage)
            .map(|entry| entry.scancode)
    }

    /// set-1 扫描码 → HID Usage（`usage_to_scancode` 的逆映射）。
    #[must_use]
    pub fn scancode_to_usage(scancode: u16) -> Option<u16> {
        TABLE
            .iter()
            .find(|entry| entry.scancode == scancode)
            .map(|entry| entry.usage)
    }

    /// 修饰键 Usage → HID 修饰位（bit0=LCtrl … bit7=RGUI）；非修饰键返回 `None`。
    #[must_use]
    pub fn modifier_bit(usage: u16) -> Option<u8> {
        match usage {
            0xE0 => Some(0x01),
            0xE1 => Some(0x02),
            0xE2 => Some(0x04),
            0xE3 => Some(0x08),
            0xE4 => Some(0x10),
            0xE5 => Some(0x20),
            0xE6 => Some(0x40),
            0xE7 => Some(0x80),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_roundtrips() {
        for entry in HidKeymap::entries() {
            assert_eq!(HidKeymap::usage_to_vk(entry.usage), Some(entry.vk));
            assert_eq!(
                HidKeymap::usage_to_scancode(entry.usage),
                Some(entry.scancode),
                "usage {:#04X} 的扫描码不一致",
                entry.usage
            );
            assert_eq!(
                HidKeymap::scancode_to_usage(entry.scancode),
                Some(entry.usage),
                "扫描码 {:#06X} 的逆映射不一致",
                entry.scancode
            );
        }
    }

    #[test]
    fn table_keys_are_unique() {
        let mut usages: Vec<u16> = HidKeymap::entries().iter().map(|e| e.usage).collect();
        let total = usages.len();
        usages.sort_unstable();
        usages.dedup();
        assert_eq!(usages.len(), total, "usage 有重复");

        let mut scancodes: Vec<u16> = HidKeymap::entries().iter().map(|e| e.scancode).collect();
        scancodes.sort_unstable();
        scancodes.dedup();
        assert_eq!(scancodes.len(), total, "scancode 有重复");
    }

    #[test]
    fn modifiers_have_bits() {
        assert_eq!(HidKeymap::modifier_bit(0xE0), Some(0x01));
        assert_eq!(HidKeymap::modifier_bit(0xE3), Some(0x08));
        assert_eq!(HidKeymap::modifier_bit(0xE7), Some(0x80));
        assert_eq!(HidKeymap::modifier_bit(0x04), None);
    }

    #[test]
    fn unknown_usages_are_none() {
        assert_eq!(HidKeymap::usage_to_vk(0x00), None);
        assert_eq!(HidKeymap::usage_to_scancode(0xFFFF), None);
        assert_eq!(HidKeymap::scancode_to_usage(0x0000), None);
    }
}
