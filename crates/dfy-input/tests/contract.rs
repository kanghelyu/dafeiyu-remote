// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Dafeiyu Remote contributors

//! `dfy-input` 对外的契约测试：映射表一致性、快捷键四态、Unicode 拆分与防卡键记录。

use std::collections::HashSet;

use dfy_input::testing::RecordingSink;
use dfy_input::{
    decide_shortcut, utf16_units, HidKeymap, InputEvent, InputSink, KeyState, MouseButton,
    ShortcutContext, ShortcutDecision, ShortcutTable,
};

#[test]
fn keymap_table_roundtrips() {
    for entry in HidKeymap::entries() {
        let usage = entry.usage;
        assert_eq!(HidKeymap::usage_to_vk(usage), Some(entry.vk));
        assert_eq!(HidKeymap::usage_to_scancode(usage), Some(entry.scancode));
        assert_eq!(HidKeymap::scancode_to_usage(entry.scancode), Some(usage));
        assert_eq!(
            HidKeymap::scancode_to_usage(HidKeymap::usage_to_scancode(usage).unwrap_or_default()),
            Some(usage)
        );
    }
}

#[test]
fn keymap_table_has_no_duplicate_usage_or_scancode() {
    let mut usages = HashSet::new();
    let mut scancodes = HashSet::new();
    for entry in HidKeymap::entries() {
        assert!(
            usages.insert(entry.usage),
            "usage 重复：{:#06X}",
            entry.usage
        );
        assert!(
            scancodes.insert(entry.scancode),
            "scancode 重复：{:#06X}",
            entry.scancode
        );
    }
}

#[test]
fn required_keys_are_mapped() {
    // (HID Usage, Windows VK)
    let required: &[(u16, u16)] = &[
        (0x04, 0x41), // A
        (0x1D, 0x5A), // Z
        (0x1E, 0x31), // 1
        (0x27, 0x30), // 0
        (0x28, 0x0D), // Enter
        (0x29, 0x1B), // Esc
        (0x2A, 0x08), // Backspace
        (0x2B, 0x09), // Tab
        (0x2C, 0x20), // Space
        (0x39, 0x14), // CapsLock
        (0x3A, 0x70), // F1
        (0x45, 0x7B), // F12
        (0x46, 0x2C), // PrintScreen
        (0x47, 0x91), // ScrollLock
        (0x48, 0x13), // Pause
        (0x4F, 0x27), // Right
        (0x50, 0x25), // Left
        (0x51, 0x28), // Down
        (0x52, 0x26), // Up
        (0x49, 0x2D), // Insert
        (0x4C, 0x2E), // Delete
        (0x4A, 0x24), // Home
        (0x4D, 0x23), // End
        (0x4B, 0x21), // PageUp
        (0x4E, 0x22), // PageDown
        (0x2D, 0xBD), // -
        (0x2E, 0xBB), // =
        (0x2F, 0xDB), // [
        (0x30, 0xDD), // ]
        (0x31, 0xDC), // \
        (0x33, 0xBA), // ;
        (0x34, 0xDE), // '
        (0x35, 0xC0), // `
        (0x36, 0xBC), // ,
        (0x37, 0xBE), // .
        (0x38, 0xBF), // /
        (0x64, 0xE2), // ISO 额外键 Non-US \|
        (0x65, 0x5D), // Application
        (0xE0, 0xA2), // LeftCtrl
        (0xE1, 0xA0), // LeftShift
        (0xE2, 0xA4), // LeftAlt
        (0xE3, 0x5B), // LeftGUI
        (0xE4, 0xA3), // RightCtrl
        (0xE5, 0xA1), // RightShift
        (0xE6, 0xA5), // RightAlt
        (0xE7, 0x5C), // RightGUI
    ];
    for &(usage, vk) in required {
        assert_eq!(
            HidKeymap::usage_to_vk(usage),
            Some(vk),
            "usage {usage:#06X}"
        );
        assert!(
            HidKeymap::usage_to_scancode(usage).is_some(),
            "usage {usage:#06X} 缺少扫描码"
        );
    }
}

#[test]
fn modifier_bits_cover_all_eight() {
    let expected: &[(u16, u8)] = &[
        (0xE0, 0x01),
        (0xE1, 0x02),
        (0xE2, 0x04),
        (0xE3, 0x08),
        (0xE4, 0x10),
        (0xE5, 0x20),
        (0xE6, 0x40),
        (0xE7, 0x80),
    ];
    for &(usage, bit) in expected {
        assert_eq!(HidKeymap::modifier_bit(usage), Some(bit));
    }
    assert_eq!(HidKeymap::modifier_bit(0x04), None);
    assert_eq!(HidKeymap::usage_to_vk(0xFFFF), None);
}

#[test]
fn shortcut_decision_semantics() {
    let table = ShortcutTable {
        controller_only: vec![0x14],      // Q
        local_function: vec![0xE3, 0x2B], // LGUI、Tab
        system_local: vec![0x29, 0x46],   // Esc、PrintScreen
    };
    let plain = ShortcutContext::default();
    let remote = ShortcutContext {
        controller_only: true,
        ..ShortcutContext::default()
    };
    let injected = ShortcutContext {
        injected_by_us: true,
        controller_only: true,
        modifiers: 0x08,
    };

    assert_eq!(
        decide_shortcut(&table, 0x04, &plain),
        ShortcutDecision::SendToRemote
    );
    assert_eq!(
        decide_shortcut(&table, 0x14, &remote),
        ShortcutDecision::SendToRemote
    );
    assert_eq!(
        decide_shortcut(&table, 0x14, &plain),
        ShortcutDecision::SendToRemote
    );
    assert_eq!(
        decide_shortcut(&table, 0x29, &plain),
        ShortcutDecision::Blocked
    );
    assert_eq!(
        decide_shortcut(&table, 0xE3, &plain),
        ShortcutDecision::LocalAndRemote
    );
    assert_eq!(
        decide_shortcut(&table, 0x29, &injected),
        ShortcutDecision::LocalOnly
    );
    assert_eq!(
        decide_shortcut(&table, 0x04, &injected),
        ShortcutDecision::LocalOnly
    );
}

#[test]
fn utf16_units_splits_surrogate_pairs() {
    assert_eq!(
        utf16_units("dfy😀"),
        vec![0x0064, 0x0066, 0x0079, 0xD83D, 0xDE00]
    );
    assert_eq!(utf16_units("中"), vec![0x4E2D]);
}

#[test]
fn recording_sink_releases_all_pressed_inputs() {
    let mut sink = RecordingSink::new();
    sink.inject(&InputEvent::Key {
        usage: 0x04,
        state: KeyState::Down,
        modifiers: 0,
    })
    .expect("key down");
    sink.inject(&InputEvent::MouseButton {
        button: MouseButton::Forward,
        state: KeyState::Down,
    })
    .expect("button down");
    assert_eq!(sink.event_count(), 2);

    sink.release_all();
    assert_eq!(sink.release_calls(), 1);
    assert_eq!(sink.event_count(), 4);
    assert_eq!(
        sink.events()[2],
        InputEvent::Key {
            usage: 0x04,
            state: KeyState::Up,
            modifiers: 0
        }
    );
    assert_eq!(
        sink.events()[3],
        InputEvent::MouseButton {
            button: MouseButton::Forward,
            state: KeyState::Up
        }
    );
}
