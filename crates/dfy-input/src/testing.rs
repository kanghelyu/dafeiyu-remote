// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 测试辅助：记录事件的 [`RecordingSink`]。
//!
//! 该实现不触碰真实系统输入，只记录注入调用，便于上层 session/UI 单测和本 crate 的
//! 契约测试验证 UTF-16 拆分、事件顺序与 `release_all` 防卡键循环。

use crate::event::{InputEvent, InputSink, PressedState};

/// 记录所有注入事件的测试用 sink。
#[derive(Debug, Default)]
pub struct RecordingSink {
    events: Vec<InputEvent>,
    release_calls: usize,
    pressed: PressedState,
}

impl RecordingSink {
    /// 创建空的记录 sink。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 已记录的注入事件（含 `release_all` 补发的 Up 事件）。
    #[must_use]
    pub fn events(&self) -> &[InputEvent] {
        &self.events
    }

    /// 事件总数。
    #[must_use]
    pub fn event_count(&self) -> usize {
        self.events.len()
    }

    /// `release_all` 被调用的次数。
    #[must_use]
    pub fn release_calls(&self) -> usize {
        self.release_calls
    }
}

impl InputSink for RecordingSink {
    fn inject(&mut self, ev: &InputEvent) -> anyhow::Result<()> {
        self.pressed.track(ev);
        self.events.push(ev.clone());
        Ok(())
    }

    fn release_all(&mut self) {
        self.release_calls += 1;
        let releases = self.pressed.release_events();
        self.events.extend(releases);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{KeyState, MouseButton};

    #[test]
    fn release_all_emits_one_up_per_pressed_input() {
        let mut sink = RecordingSink::new();
        sink.inject(&InputEvent::Key {
            usage: 0x04,
            state: KeyState::Down,
            modifiers: 0,
        })
        .expect("record key down");
        // 重复 Down 不应重复补发。
        sink.inject(&InputEvent::Key {
            usage: 0x04,
            state: KeyState::Down,
            modifiers: 0,
        })
        .expect("record repeated key down");
        sink.inject(&InputEvent::Key {
            usage: 0x05,
            state: KeyState::Down,
            modifiers: 0,
        })
        .expect("record second key down");
        sink.inject(&InputEvent::MouseButton {
            button: MouseButton::Left,
            state: KeyState::Down,
        })
        .expect("record button down");
        assert_eq!(sink.release_calls(), 0);

        sink.release_all();
        assert_eq!(sink.release_calls(), 1);
        assert_eq!(sink.event_count(), 7);
        let releases = &sink.events()[4..];
        assert_eq!(releases.len(), 3);
        assert_eq!(
            releases[0],
            InputEvent::Key {
                usage: 0x04,
                state: KeyState::Up,
                modifiers: 0
            }
        );
        assert_eq!(
            releases[1],
            InputEvent::Key {
                usage: 0x05,
                state: KeyState::Up,
                modifiers: 0
            }
        );
        assert_eq!(
            releases[2],
            InputEvent::MouseButton {
                button: MouseButton::Left,
                state: KeyState::Up
            }
        );

        // 再次调用不应产生新事件（已清空按下状态）。
        sink.release_all();
        assert_eq!(sink.release_calls(), 2);
        assert_eq!(sink.event_count(), 7);
    }

    #[test]
    fn text_events_are_recorded_verbatim() {
        let mut sink = RecordingSink::new();
        sink.inject(&InputEvent::Text {
            text: "中文😀".to_owned(),
        })
        .expect("record text");
        assert_eq!(
            sink.events(),
            &[InputEvent::Text {
                text: "中文😀".to_owned()
            }]
        );
        assert!(sink.pressed.is_empty());
    }
}
