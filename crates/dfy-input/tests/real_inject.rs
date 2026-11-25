// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 真实注入测试：**默认全部 `#[ignore]`**，因为会真的移动鼠标、发送按键。
//!
//! 手动运行：`cargo test -p dfy-input --test real_inject -- --ignored --test-threads=1`
#![cfg(windows)]

use dfy_input::{create_sink, InputEvent, KeyState, MouseButton};

#[test]
#[ignore = "真实注入，会移动鼠标指针"]
fn real_absolute_mouse_move() -> anyhow::Result<()> {
    let mut sink = create_sink()?;
    sink.inject(&InputEvent::MouseMoveAbs {
        x: 100,
        y: 100,
        width: 1920,
        height: 1080,
    })?;
    sink.inject(&InputEvent::MouseMoveRel { dx: 1, dy: 1 })?;
    sink.inject(&InputEvent::Wheel { dx: 0, dy: 0 })?;
    sink.release_all();
    Ok(())
}

#[test]
#[ignore = "真实注入，会发送按键/文本"]
fn real_text_and_key_injection() -> anyhow::Result<()> {
    let mut sink = create_sink()?;
    sink.inject(&InputEvent::Text {
        text: "dfy✓".to_owned(),
    })?;
    sink.inject(&InputEvent::Key {
        usage: 0x04,
        state: KeyState::Down,
        modifiers: 0,
    })?;
    sink.inject(&InputEvent::Key {
        usage: 0x04,
        state: KeyState::Up,
        modifiers: 0,
    })?;
    sink.inject(&InputEvent::MouseButton {
        button: MouseButton::Middle,
        state: KeyState::Down,
    })?;
    sink.inject(&InputEvent::MouseButton {
        button: MouseButton::Middle,
        state: KeyState::Up,
    })?;
    sink.release_all();
    Ok(())
}

#[test]
#[ignore = "真实注入，会按下并补发释放按键"]
fn real_release_all_guards_stuck_keys() -> anyhow::Result<()> {
    let mut sink = create_sink()?;
    sink.inject(&InputEvent::Key {
        usage: 0xE1,
        state: KeyState::Down,
        modifiers: 0x02,
    })?;
    sink.release_all();
    Ok(())
}
