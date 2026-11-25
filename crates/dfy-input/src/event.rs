// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 输入事件、注入接口与「按下状态」跟踪。
//!
//! 控制端采集原始输入后编码为 [`InputEvent`]，经 `dfy-proto` 的 input 通道送到被控端，
//! 由被控端的 [`InputSink`] 实现注入系统。键位一律使用 HID Usage ID
//! （Keyboard/Keypad page `0x07`），平台差异由 [`crate::keymap`] 负责映射。

/// 按键状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyState {
    /// 按下。
    Down,
    /// 抬起。
    Up,
}

/// 鼠标按键。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    /// 左键。
    Left,
    /// 右键。
    Right,
    /// 中键。
    Middle,
    /// 后退（XButton1）。
    Back,
    /// 前进（XButton2）。
    Forward,
}

/// 单个输入事件。
///
/// `usage` 为 HID Usage ID（Keyboard/Keypad page `0x07`），不是 Windows VK，
/// 也不是扫描码；VK/扫描码由 [`crate::HidKeymap`] 转换。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    /// 键盘按键。`modifiers` 为 HID 修饰键位图（bit0=LCtrl … bit7=RGUI），
    /// 仅供上层诊断/回放参考，注入端不会据此臆造修饰键状态。
    Key {
        /// HID Usage ID。
        usage: u16,
        /// 按下或抬起。
        state: KeyState,
        /// 当前修饰键位图（HID 语义）。
        modifiers: u8,
    },
    /// 鼠标按键。
    MouseButton {
        /// 哪个按键。
        button: MouseButton,
        /// 按下或抬起。
        state: KeyState,
    },
    /// 相对鼠标移动（像素）。
    MouseMoveRel {
        /// 水平位移。
        dx: i16,
        /// 垂直位移。
        dy: i16,
    },
    /// 绝对鼠标移动：目标端像素坐标。
    ///
    /// `x`/`y` 是采集画面内的像素坐标，`width`/`height` 是画面尺寸；画面覆盖
    /// 整个虚拟桌面时（多屏/虚拟显示器场景），实现端按该尺寸归一化到 `0..=65535`。
    MouseMoveAbs {
        /// 目标像素横坐标。
        x: u32,
        /// 目标像素纵坐标。
        y: u32,
        /// 画面宽度（像素）。
        width: u32,
        /// 画面高度（像素）。
        height: u32,
    },
    /// 滚轮：单位为 Windows `WHEEL_DELTA`（`120` = 一格）；`dy` 正值向上，
    /// `dx` 正值向右。
    Wheel {
        /// 水平滚动增量。
        dx: i16,
        /// 垂直滚动增量。
        dy: i16,
    },
    /// 文本注入（IME/Unicode），绕开键盘布局直接输入字符。
    Text {
        /// 要注入的文本。
        text: String,
    },
}

/// 输入注入后端（被控端实现）。
pub trait InputSink: Send {
    /// 注入一个事件。实现应当尽量把同一事件的多个底层事件合并为一次系统调用。
    fn inject(&mut self, ev: &InputEvent) -> anyhow::Result<()>;

    /// 释放所有仍处于按下状态的键与鼠标键（会话断开/结束时必须调用）。
    fn release_all(&mut self);
}

/// 把文本按 UTF-16 码元拆分（Windows `KEYEVENTF_UNICODE` 需要逐码元注入，
/// 补充平面字符会被拆成代理对）。非 Windows 后端做文本注入时也复用该逻辑。
#[must_use]
pub fn utf16_units(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

/// 跟踪当前仍按下的键与鼠标键，供 [`InputSink::release_all`] 补发 Up 事件。
///
/// 真实后端与测试用的 [`crate::testing::RecordingSink`] 共用该逻辑，保证
/// 「防卡键」行为可被单元测试覆盖。
#[derive(Debug, Default)]
pub struct PressedState {
    keys: Vec<u16>,
    buttons: Vec<MouseButton>,
}

impl PressedState {
    /// 创建空状态。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 根据事件更新按下状态；重复的 Down 不会重复记录。
    pub fn track(&mut self, ev: &InputEvent) {
        match ev {
            InputEvent::Key {
                usage,
                state: KeyState::Down,
                ..
            } => {
                if !self.keys.contains(usage) {
                    self.keys.push(*usage);
                }
            }
            InputEvent::Key {
                usage,
                state: KeyState::Up,
                ..
            } => self.keys.retain(|k| k != usage),
            InputEvent::MouseButton {
                button,
                state: KeyState::Down,
            } => {
                if !self.buttons.contains(button) {
                    self.buttons.push(*button);
                }
            }
            InputEvent::MouseButton {
                button,
                state: KeyState::Up,
            } => self.buttons.retain(|b| b != button),
            _ => {}
        }
    }

    /// 取出所有需要补发的 Up 事件（先键盘、后鼠标键），并清空按下状态。
    pub fn release_events(&mut self) -> Vec<InputEvent> {
        let mut out = Vec::with_capacity(self.keys.len() + self.buttons.len());
        for usage in self.keys.drain(..) {
            out.push(InputEvent::Key {
                usage,
                state: KeyState::Up,
                modifiers: 0,
            });
        }
        for button in self.buttons.drain(..) {
            out.push(InputEvent::MouseButton {
                button,
                state: KeyState::Up,
            });
        }
        out
    }

    /// 当前按下的 HID Usage 列表。
    #[must_use]
    pub fn pressed_keys(&self) -> &[u16] {
        &self.keys
    }

    /// 当前按下的鼠标键列表。
    #[must_use]
    pub fn pressed_buttons(&self) -> &[MouseButton] {
        &self.buttons
    }

    /// 是否没有任何按下的键/鼠标键。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.buttons.is_empty()
    }
}

/// 输入注入失败分类；对外接口仍统一返回 [`anyhow::Result`]。
#[derive(Debug, thiserror::Error)]
pub enum InputError {
    /// 当前平台没有可用的注入后端。
    #[error("当前平台不支持输入注入（仅 Windows 后端已实现）")]
    Unsupported,
    /// 系统注入 API 失败。
    #[error("输入注入失败：{0}")]
    Send(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pressed_state_tracks_and_releases() {
        let mut st = PressedState::new();
        st.track(&InputEvent::Key {
            usage: 0x04,
            state: KeyState::Down,
            modifiers: 0,
        });
        st.track(&InputEvent::Key {
            usage: 0x04,
            state: KeyState::Down,
            modifiers: 0,
        });
        st.track(&InputEvent::MouseButton {
            button: MouseButton::Back,
            state: KeyState::Down,
        });
        assert_eq!(st.pressed_keys(), &[0x04]);
        assert_eq!(st.pressed_buttons(), &[MouseButton::Back]);

        let releases = st.release_events();
        assert_eq!(releases.len(), 2);
        assert!(st.is_empty());
        st.track(&InputEvent::Key {
            usage: 0x04,
            state: KeyState::Up,
            modifiers: 0,
        });
        assert!(st.is_empty());
    }

    #[test]
    fn utf16_splits_surrogate_pairs() {
        assert_eq!(utf16_units("a😀"), vec![0x0061, 0xD83D, 0xDE00]);
        assert_eq!(utf16_units("中"), vec![0x4E2D]);
        assert!(utf16_units("").is_empty());
    }
}
