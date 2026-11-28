// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! USB HID Keyboard/Keypad（page `0x07`）↔ macOS `CGKeyCode`（`HIToolbox/Events.h`）双向映射。
//!
//! 故意不加 `#[cfg(target_os = "macos")]`，使 Linux/Windows CI 也能执行完整单测，
//! 提前发现重复键码或回归。

/// 将 HID Keyboard/Keypad Usage ID（page `0x07`）转换为 macOS `CGKeyCode`。
#[must_use]
pub const fn macos_usage_to_keycode(usage: u16) -> Option<u16> {
    match usage {
        // Letters A..=Z (HIToolbox kVK_ANSI_*)
        0x04 => Some(0x00), // A
        0x05 => Some(0x0B), // B
        0x06 => Some(0x08), // C
        0x07 => Some(0x02), // D
        0x08 => Some(0x0E), // E
        0x09 => Some(0x03), // F
        0x0A => Some(0x05), // G
        0x0B => Some(0x04), // H
        0x0C => Some(0x22), // I
        0x0D => Some(0x26), // J
        0x0E => Some(0x28), // K
        0x0F => Some(0x25), // L
        0x10 => Some(0x2E), // M
        0x11 => Some(0x2D), // N
        0x12 => Some(0x1F), // O
        0x13 => Some(0x23), // P
        0x14 => Some(0x0C), // Q
        0x15 => Some(0x0F), // R
        0x16 => Some(0x01), // S
        0x17 => Some(0x11), // T
        0x18 => Some(0x20), // U
        0x19 => Some(0x09), // V
        0x1A => Some(0x0D), // W
        0x1B => Some(0x07), // X
        0x1C => Some(0x10), // Y
        0x1D => Some(0x06), // Z

        // Digits 1..=9, 0
        0x1E => Some(0x12), // 1
        0x1F => Some(0x13), // 2
        0x20 => Some(0x14), // 3
        0x21 => Some(0x15), // 4
        0x22 => Some(0x17), // 5
        0x23 => Some(0x16), // 6
        0x24 => Some(0x1A), // 7
        0x25 => Some(0x1C), // 8
        0x26 => Some(0x19), // 9
        0x27 => Some(0x1D), // 0

        // Controls & punctuation
        0x28 => Some(0x24), // Return
        0x29 => Some(0x35), // Escape
        0x2A => Some(0x33), // Backspace (kVK_Delete)
        0x2B => Some(0x30), // Tab
        0x2C => Some(0x31), // Space
        0x2D => Some(0x1B), // Minus
        0x2E => Some(0x18), // Equal
        0x2F => Some(0x21), // LeftBracket
        0x30 => Some(0x1E), // RightBracket
        0x31 => Some(0x2A), // Backslash
        0x32 => Some(0x2A), // Non-US # and ~ (maps to ANSI Backslash)
        0x33 => Some(0x29), // Semicolon
        0x34 => Some(0x27), // Quote
        0x35 => Some(0x32), // Grave
        0x36 => Some(0x2B), // Comma
        0x37 => Some(0x2F), // Period
        0x38 => Some(0x2C), // Slash
        0x39 => Some(0x39), // CapsLock

        // Function keys F1..=F12
        0x3A => Some(0x7A), // F1
        0x3B => Some(0x78), // F2
        0x3C => Some(0x63), // F3
        0x3D => Some(0x76), // F4
        0x3E => Some(0x60), // F5
        0x3F => Some(0x61), // F6
        0x40 => Some(0x62), // F7
        0x41 => Some(0x64), // F8
        0x42 => Some(0x65), // F9
        0x43 => Some(0x6D), // F10
        0x44 => Some(0x67), // F11
        0x45 => Some(0x6F), // F12

        // Navigation
        0x49 => Some(0x72), // Insert / Help (kVK_Help)
        0x4A => Some(0x73), // Home
        0x4B => Some(0x74), // PageUp
        0x4C => Some(0x75), // ForwardDelete
        0x4D => Some(0x77), // End
        0x4E => Some(0x79), // PageDown
        0x4F => Some(0x7C), // RightArrow
        0x50 => Some(0x7B), // LeftArrow
        0x51 => Some(0x7D), // DownArrow
        0x52 => Some(0x7E), // UpArrow

        // Keypad
        0x53 => Some(0x47), // KeypadClear / NumLock
        0x54 => Some(0x4B), // KeypadDivide
        0x55 => Some(0x43), // KeypadMultiply
        0x56 => Some(0x4E), // KeypadMinus
        0x57 => Some(0x45), // KeypadPlus
        0x58 => Some(0x4C), // KeypadEnter
        0x59 => Some(0x53), // Keypad1
        0x5A => Some(0x54), // Keypad2
        0x5B => Some(0x55), // Keypad3
        0x5C => Some(0x56), // Keypad4
        0x5D => Some(0x57), // Keypad5
        0x5E => Some(0x58), // Keypad6
        0x5F => Some(0x59), // Keypad7
        0x60 => Some(0x5B), // Keypad8
        0x61 => Some(0x5C), // Keypad9
        0x62 => Some(0x52), // Keypad0
        0x63 => Some(0x41), // KeypadDecimal
        0x67 => Some(0x51), // KeypadEquals

        // ISO / Menu / F13..=F20 / Audio
        0x64 => Some(0x0A), // ISO Section
        0x65 => Some(0x6E), // ContextualMenu
        0x68 => Some(0x69), // F13
        0x69 => Some(0x6B), // F14
        0x6A => Some(0x71), // F15
        0x6B => Some(0x6A), // F16
        0x6C => Some(0x40), // F17
        0x6D => Some(0x4F), // F18
        0x6E => Some(0x50), // F19
        0x6F => Some(0x5A), // F20
        0x7F => Some(0x4A), // Mute
        0x80 => Some(0x48), // VolumeUp
        0x81 => Some(0x49), // VolumeDown

        // Left/Right Modifiers
        0xE0 => Some(0x3B), // LCtrl
        0xE1 => Some(0x38), // LShift
        0xE2 => Some(0x3A), // LAlt / LOption
        0xE3 => Some(0x37), // LGui / LCommand
        0xE4 => Some(0x3E), // RCtrl
        0xE5 => Some(0x3C), // RShift
        0xE6 => Some(0x3D), // RAlt / ROption
        0xE7 => Some(0x36), // RGui / RCommand

        _ => None,
    }
}

/// 将 macOS `CGKeyCode` 转回 HID Keyboard/Keypad Usage ID（page `0x07`）。
///
/// 对共享同一物理键码的 ISO `#/~` (`0x32`) 与 ANSI `\` (`0x31`) 统一返回规范值 `0x31`。
#[must_use]
pub const fn macos_keycode_to_usage(keycode: u16) -> Option<u16> {
    match keycode {
        // Letters A..=Z
        0x00 => Some(0x04),
        0x0B => Some(0x05),
        0x08 => Some(0x06),
        0x02 => Some(0x07),
        0x0E => Some(0x08),
        0x03 => Some(0x09),
        0x05 => Some(0x0A),
        0x04 => Some(0x0B),
        0x22 => Some(0x0C),
        0x26 => Some(0x0D),
        0x28 => Some(0x0E),
        0x25 => Some(0x0F),
        0x2E => Some(0x10),
        0x2D => Some(0x11),
        0x1F => Some(0x12),
        0x23 => Some(0x13),
        0x0C => Some(0x14),
        0x0F => Some(0x15),
        0x01 => Some(0x16),
        0x11 => Some(0x17),
        0x20 => Some(0x18),
        0x09 => Some(0x19),
        0x0D => Some(0x1A),
        0x07 => Some(0x1B),
        0x10 => Some(0x1C),
        0x06 => Some(0x1D),

        // Digits 1..=9, 0
        0x12 => Some(0x1E),
        0x13 => Some(0x1F),
        0x14 => Some(0x20),
        0x15 => Some(0x21),
        0x17 => Some(0x22),
        0x16 => Some(0x23),
        0x1A => Some(0x24),
        0x1C => Some(0x25),
        0x19 => Some(0x26),
        0x1D => Some(0x27),

        // Controls & punctuation
        0x24 => Some(0x28),
        0x35 => Some(0x29),
        0x33 => Some(0x2A),
        0x30 => Some(0x2B),
        0x31 => Some(0x2C),
        0x1B => Some(0x2D),
        0x18 => Some(0x2E),
        0x21 => Some(0x2F),
        0x1E => Some(0x30),
        0x2A => Some(0x31),
        0x29 => Some(0x33),
        0x27 => Some(0x34),
        0x32 => Some(0x35),
        0x2B => Some(0x36),
        0x2F => Some(0x37),
        0x2C => Some(0x38),
        0x39 => Some(0x39),

        // Function keys F1..=F12
        0x7A => Some(0x3A),
        0x78 => Some(0x3B),
        0x63 => Some(0x3C),
        0x76 => Some(0x3D),
        0x60 => Some(0x3E),
        0x61 => Some(0x3F),
        0x62 => Some(0x40),
        0x64 => Some(0x41),
        0x65 => Some(0x42),
        0x6D => Some(0x43),
        0x67 => Some(0x44),
        0x6F => Some(0x45),

        // Navigation
        0x72 => Some(0x49),
        0x73 => Some(0x4A),
        0x74 => Some(0x4B),
        0x75 => Some(0x4C),
        0x77 => Some(0x4D),
        0x79 => Some(0x4E),
        0x7C => Some(0x4F),
        0x7B => Some(0x50),
        0x7D => Some(0x51),
        0x7E => Some(0x52),

        // Keypad
        0x47 => Some(0x53),
        0x4B => Some(0x54),
        0x43 => Some(0x55),
        0x4E => Some(0x56),
        0x45 => Some(0x57),
        0x4C => Some(0x58),
        0x53 => Some(0x59),
        0x54 => Some(0x5A),
        0x55 => Some(0x5B),
        0x56 => Some(0x5C),
        0x57 => Some(0x5D),
        0x58 => Some(0x5E),
        0x59 => Some(0x5F),
        0x5B => Some(0x60),
        0x5C => Some(0x61),
        0x52 => Some(0x62),
        0x41 => Some(0x63),
        0x51 => Some(0x67),

        // ISO / Menu / F13..=F20 / Audio
        0x0A => Some(0x64),
        0x6E => Some(0x65),
        0x69 => Some(0x68),
        0x6B => Some(0x69),
        0x71 => Some(0x6A),
        0x6A => Some(0x6B),
        0x40 => Some(0x6C),
        0x4F => Some(0x6D),
        0x50 => Some(0x6E),
        0x5A => Some(0x6F),
        0x4A => Some(0x7F),
        0x48 => Some(0x80),
        0x49 => Some(0x81),

        // Modifiers
        0x3B => Some(0xE0),
        0x38 => Some(0xE1),
        0x3A => Some(0xE2),
        0x37 => Some(0xE3),
        0x3E => Some(0xE4),
        0x3C => Some(0xE5),
        0x3D => Some(0xE6),
        0x36 => Some(0xE7),

        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn roundtrip_all_non_alias_usages_and_no_unintended_duplicates() {
        let mut seen_keycodes: HashMap<u16, u16> = HashMap::new();
        for usage in 0..=0xFFu16 {
            if let Some(keycode) = macos_usage_to_keycode(usage) {
                if usage == 0x32 {
                    // HID 0x32 (Non-US #/~) intentionally aliases ANSI backslash 0x2A -> 0x31.
                    assert_eq!(keycode, 0x2A);
                    assert_eq!(macos_keycode_to_usage(keycode), Some(0x31));
                    continue;
                }
                if let Some(prev) = seen_keycodes.insert(keycode, usage) {
                    panic!(
                        "duplicate macOS CGKeyCode 0x{keycode:02X} for HID usages 0x{prev:02X} and 0x{usage:02X}"
                    );
                }
                assert_eq!(
                    macos_keycode_to_usage(keycode),
                    Some(usage),
                    "keycode 0x{keycode:02X} did not invert back to HID usage 0x{usage:02X}"
                );
            }
        }
        // Verify full 88+ entries are covered including modifiers and function keys.
        assert!(seen_keycodes.len() >= 90);
    }

    #[test]
    fn unknown_usages_and_keycodes_return_none() {
        assert_eq!(macos_usage_to_keycode(0x00), None);
        assert_eq!(macos_usage_to_keycode(0xFFFF), None);
        assert_eq!(macos_keycode_to_usage(0xFF), None);
        assert_eq!(macos_keycode_to_usage(0x0100), None);
    }

    #[test]
    fn spot_check_critical_mappings() {
        assert_eq!(macos_usage_to_keycode(0x04), Some(0x00)); // A
        assert_eq!(macos_usage_to_keycode(0x14), Some(0x0C)); // Q
        assert_eq!(macos_usage_to_keycode(0x28), Some(0x24)); // Return
        assert_eq!(macos_usage_to_keycode(0x29), Some(0x35)); // Escape
        assert_eq!(macos_usage_to_keycode(0xE0), Some(0x3B)); // LCtrl
        assert_eq!(macos_usage_to_keycode(0xE4), Some(0x3E)); // RCtrl
        assert_eq!(macos_usage_to_keycode(0xE3), Some(0x37)); // LCmd
        assert_eq!(macos_usage_to_keycode(0xE7), Some(0x36)); // RCmd
    }
}
