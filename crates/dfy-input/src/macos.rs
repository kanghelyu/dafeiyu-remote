// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! macOS `CGEvent` 输入注入后端。
//!
//! 依赖 macOS 辅助功能（Accessibility）权限，并在 `release_all`/`Drop` 时自动补发抬起事件防卡键。

use std::ffi::c_void;

use anyhow::Result;
use tracing::debug;

use crate::event::{
    utf16_units, InputError, InputEvent, InputSink, KeyState, MouseButton, PressedState,
};
use crate::macos_keymap::macos_usage_to_keycode;

const K_CG_HID_EVENT_TAP: u32 = 0;
const K_CG_EVENT_SOURCE_STATE_HID_SYSTEM_STATE: i32 = 1;

const K_CG_EVENT_LEFT_MOUSE_DOWN: u32 = 1;
const K_CG_EVENT_LEFT_MOUSE_UP: u32 = 2;
const K_CG_EVENT_RIGHT_MOUSE_DOWN: u32 = 3;
const K_CG_EVENT_RIGHT_MOUSE_UP: u32 = 4;
const K_CG_EVENT_MOUSE_MOVED: u32 = 5;
const K_CG_EVENT_LEFT_MOUSE_DRAGGED: u32 = 6;
const K_CG_EVENT_RIGHT_MOUSE_DRAGGED: u32 = 7;
const K_CG_EVENT_OTHER_MOUSE_DOWN: u32 = 25;
const K_CG_EVENT_OTHER_MOUSE_UP: u32 = 26;
const K_CG_EVENT_OTHER_MOUSE_DRAGGED: u32 = 27;

const K_CG_MOUSE_BUTTON_LEFT: u32 = 0;
const K_CG_MOUSE_BUTTON_RIGHT: u32 = 1;
const K_CG_MOUSE_BUTTON_CENTER: u32 = 2;

const K_CG_SCROLL_EVENT_UNIT_LINE: u32 = 1;

const K_CG_MOUSE_EVENT_BUTTON_NUMBER: u32 = 3;
const K_CG_MOUSE_EVENT_DELTA_X: u32 = 4;
const K_CG_MOUSE_EVENT_DELTA_Y: u32 = 5;

const K_CG_EVENT_FLAG_MASK_ALPHA_SHIFT: u64 = 0x0001_0000;
const K_CG_EVENT_FLAG_MASK_SHIFT: u64 = 0x0002_0000;
const K_CG_EVENT_FLAG_MASK_CONTROL: u64 = 0x0004_0000;
const K_CG_EVENT_FLAG_MASK_ALTERNATE: u64 = 0x0008_0000;
const K_CG_EVENT_FLAG_MASK_COMMAND: u64 = 0x0010_0000;

const NX_DEVICELCTLKEYMASK: u64 = 0x0000_0001;
const NX_DEVICELSHIFTKEYMASK: u64 = 0x0000_0002;
const NX_DEVICERSHIFTKEYMASK: u64 = 0x0000_0004;
const NX_DEVICELCMDKEYMASK: u64 = 0x0000_0008;
const NX_DEVICERCMDKEYMASK: u64 = 0x0000_0010;
const NX_DEVICELALTKEYMASK: u64 = 0x0000_0020;
const NX_DEVICERALTKEYMASK: u64 = 0x0000_0040;
const NX_DEVICERCTLKEYMASK: u64 = 0x0000_2000;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct CGSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

#[allow(clippy::duplicated_attributes)]
#[link(name = "ApplicationServices", kind = "framework")]
#[link(name = "CoreFoundation", kind = "framework")]
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn CFRelease(cf: *const c_void);

    fn CGMainDisplayID() -> u32;
    fn CGDisplayBounds(display: u32) -> CGRect;
    fn CGEventSourceCreate(state_id: i32) -> *mut c_void;
    fn CGEventCreate(source: *mut c_void) -> *mut c_void;
    fn CGEventGetLocation(event: *const c_void) -> CGPoint;
    fn CGEventCreateKeyboardEvent(
        source: *mut c_void,
        virtual_key: u16,
        key_down: bool,
    ) -> *mut c_void;
    fn CGEventKeyboardSetUnicodeString(
        event: *mut c_void,
        string_length: usize,
        unicode_string: *const u16,
    );
    fn CGEventCreateMouseEvent(
        source: *mut c_void,
        mouse_type: u32,
        mouse_cursor_position: CGPoint,
        mouse_button: u32,
    ) -> *mut c_void;
    fn CGEventCreateScrollWheelEvent2(
        source: *mut c_void,
        units: u32,
        wheel_count: u32,
        wheel1: i32,
        wheel2: i32,
        wheel3: i32,
    ) -> *mut c_void;
    fn CGEventSetFlags(event: *mut c_void, flags: u64);
    fn CGEventSetIntegerValueField(event: *mut c_void, field: u32, value: i64);
    fn CGEventPost(tap: u32, event: *mut c_void);
}

struct EventSourceGuard(*mut c_void);

// SAFETY: CGEventSourceRef is a thread-safe CF type after construction; MacInputSink is moved to
// the single Agent worker mutex and released deterministically on Drop.
unsafe impl Send for EventSourceGuard {}

impl Drop for EventSourceGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: self.0 was created via CGEventSourceCreate and not yet released.
            unsafe { CFRelease(self.0) };
            self.0 = std::ptr::null_mut();
        }
    }
}

/// 创建 macOS `CGEvent` 输入注入后端。
pub(crate) fn create_sink() -> Result<Box<dyn InputSink>> {
    // SAFETY: AXIsProcessTrusted takes no pointers and queries current process TCC status.
    if unsafe { !AXIsProcessTrusted() } {
        return Err(InputError::Unavailable(
            "缺少 macOS 辅助功能权限（Accessibility）；请在「系统设置 → 隐私与安全性 → 辅助功能」中允许当前应用或终端后再启用 --allow-input".to_string(),
        )
        .into());
    }
    // SAFETY: K_CG_EVENT_SOURCE_STATE_HID_SYSTEM_STATE is a documented CGEventSourceStateID.
    let source =
        EventSourceGuard(unsafe { CGEventSourceCreate(K_CG_EVENT_SOURCE_STATE_HID_SYSTEM_STATE) });
    let cursor = query_current_cursor(source.0);
    Ok(Box::new(MacInputSink {
        source,
        pressed: PressedState::new(),
        cursor,
    }))
}

fn query_current_cursor(source: *mut c_void) -> CGPoint {
    // SAFETY: CGEventCreate accepts nullable source; if non-null, CGEventGetLocation reads cursor
    // and CFRelease frees it immediately.
    unsafe {
        let ev = CGEventCreate(source);
        if ev.is_null() {
            return CGPoint::default();
        }
        let pos = CGEventGetLocation(ev);
        CFRelease(ev);
        pos
    }
}

struct MacInputSink {
    source: EventSourceGuard,
    pressed: PressedState,
    cursor: CGPoint,
}

impl MacInputSink {
    fn main_display_bounds(&self) -> CGRect {
        // SAFETY: CGMainDisplayID and CGDisplayBounds have no precondition and return valid bounds.
        let rect = unsafe { CGDisplayBounds(CGMainDisplayID()) };
        if rect.size.width > 0.0 && rect.size.height > 0.0 {
            rect
        } else {
            CGRect {
                origin: CGPoint { x: 0.0, y: 0.0 },
                size: CGSize {
                    width: 1920.0,
                    height: 1080.0,
                },
            }
        }
    }

    fn current_flags(&self) -> u64 {
        modifier_flags_from_keys(self.pressed.pressed_keys())
    }

    fn drag_or_move_type(&self) -> (u32, u32) {
        match self.pressed.pressed_buttons().last() {
            Some(MouseButton::Left) => (K_CG_EVENT_LEFT_MOUSE_DRAGGED, K_CG_MOUSE_BUTTON_LEFT),
            Some(MouseButton::Right) => (K_CG_EVENT_RIGHT_MOUSE_DRAGGED, K_CG_MOUSE_BUTTON_RIGHT),
            Some(MouseButton::Middle) => (K_CG_EVENT_OTHER_MOUSE_DRAGGED, K_CG_MOUSE_BUTTON_CENTER),
            Some(MouseButton::Back) => (K_CG_EVENT_OTHER_MOUSE_DRAGGED, 3),
            Some(MouseButton::Forward) => (K_CG_EVENT_OTHER_MOUSE_DRAGGED, 4),
            None => (K_CG_EVENT_MOUSE_MOVED, K_CG_MOUSE_BUTTON_LEFT),
        }
    }

    fn post_mouse_move(&self, pos: CGPoint, dx: i64, dy: i64) -> Result<()> {
        let (event_type, button) = self.drag_or_move_type();
        // SAFETY: source pointer is valid (or null); created CGEventRef is posted and released.
        unsafe {
            let ev = CGEventCreateMouseEvent(self.source.0, event_type, pos, button);
            if ev.is_null() {
                return Err(InputError::Send("CGEventCreateMouseEvent 返回空句柄".into()).into());
            }
            if button >= K_CG_MOUSE_BUTTON_CENTER {
                CGEventSetIntegerValueField(ev, K_CG_MOUSE_EVENT_BUTTON_NUMBER, i64::from(button));
            }
            CGEventSetIntegerValueField(ev, K_CG_MOUSE_EVENT_DELTA_X, dx);
            CGEventSetIntegerValueField(ev, K_CG_MOUSE_EVENT_DELTA_Y, dy);
            CGEventSetFlags(ev, self.current_flags());
            CGEventPost(K_CG_HID_EVENT_TAP, ev);
            CFRelease(ev);
        }
        Ok(())
    }
}

impl Drop for MacInputSink {
    fn drop(&mut self) {
        self.release_all();
    }
}

impl InputSink for MacInputSink {
    fn inject(&mut self, ev: &InputEvent) -> Result<()> {
        match ev {
            InputEvent::Key { usage, state, .. } => {
                let keycode = macos_usage_to_keycode(*usage).ok_or_else(|| {
                    InputError::UnsupportedEvent(format!("不支持的 HID Usage 0x{usage:04x}"))
                })?;
                self.pressed.track(ev);
                let flags = self.current_flags();
                let down = *state == KeyState::Down;
                // SAFETY: source is valid/null; CGEventRef is checked non-null, posted, then released.
                unsafe {
                    let event = CGEventCreateKeyboardEvent(self.source.0, keycode, down);
                    if event.is_null() {
                        return Err(InputError::Send(
                            "CGEventCreateKeyboardEvent 返回空句柄".into(),
                        )
                        .into());
                    }
                    CGEventSetFlags(event, flags);
                    CGEventPost(K_CG_HID_EVENT_TAP, event);
                    CFRelease(event);
                }
                Ok(())
            }
            InputEvent::MouseButton { button, state } => {
                let down = *state == KeyState::Down;
                let (event_type, cg_button, button_number) = match button {
                    MouseButton::Left => (
                        if down {
                            K_CG_EVENT_LEFT_MOUSE_DOWN
                        } else {
                            K_CG_EVENT_LEFT_MOUSE_UP
                        },
                        K_CG_MOUSE_BUTTON_LEFT,
                        None,
                    ),
                    MouseButton::Right => (
                        if down {
                            K_CG_EVENT_RIGHT_MOUSE_DOWN
                        } else {
                            K_CG_EVENT_RIGHT_MOUSE_UP
                        },
                        K_CG_MOUSE_BUTTON_RIGHT,
                        None,
                    ),
                    MouseButton::Middle => (
                        if down {
                            K_CG_EVENT_OTHER_MOUSE_DOWN
                        } else {
                            K_CG_EVENT_OTHER_MOUSE_UP
                        },
                        K_CG_MOUSE_BUTTON_CENTER,
                        Some(2i64),
                    ),
                    MouseButton::Back => (
                        if down {
                            K_CG_EVENT_OTHER_MOUSE_DOWN
                        } else {
                            K_CG_EVENT_OTHER_MOUSE_UP
                        },
                        3,
                        Some(3i64),
                    ),
                    MouseButton::Forward => (
                        if down {
                            K_CG_EVENT_OTHER_MOUSE_DOWN
                        } else {
                            K_CG_EVENT_OTHER_MOUSE_UP
                        },
                        4,
                        Some(4i64),
                    ),
                };
                self.pressed.track(ev);
                let flags = self.current_flags();
                // SAFETY: source is valid/null; mouse event is posted at self.cursor and released.
                unsafe {
                    let event =
                        CGEventCreateMouseEvent(self.source.0, event_type, self.cursor, cg_button);
                    if event.is_null() {
                        return Err(
                            InputError::Send("CGEventCreateMouseEvent 返回空句柄".into()).into(),
                        );
                    }
                    if let Some(num) = button_number {
                        CGEventSetIntegerValueField(event, K_CG_MOUSE_EVENT_BUTTON_NUMBER, num);
                    }
                    CGEventSetFlags(event, flags);
                    CGEventPost(K_CG_HID_EVENT_TAP, event);
                    CFRelease(event);
                }
                Ok(())
            }
            InputEvent::MouseMoveAbs {
                x,
                y,
                width,
                height,
            } => {
                let bounds = self.main_display_bounds();
                let (nx, ny) = map_abs_to_rect(*x, *y, *width, *height, bounds);
                let dx = (nx - self.cursor.x).round() as i64;
                let dy = (ny - self.cursor.y).round() as i64;
                let pos = CGPoint { x: nx, y: ny };
                self.cursor = pos;
                self.post_mouse_move(pos, dx, dy)
            }
            InputEvent::MouseMoveRel { dx, dy } => {
                let bounds = self.main_display_bounds();
                let max_x = bounds.origin.x + (bounds.size.width - 1.0).max(0.0);
                let max_y = bounds.origin.y + (bounds.size.height - 1.0).max(0.0);
                let pos = CGPoint {
                    x: (self.cursor.x + f64::from(*dx)).clamp(bounds.origin.x, max_x),
                    y: (self.cursor.y + f64::from(*dy)).clamp(bounds.origin.y, max_y),
                };
                self.cursor = pos;
                self.post_mouse_move(pos, i64::from(*dx), i64::from(*dy))
            }
            InputEvent::Wheel { dx, dy } => {
                let v_lines = wheel_delta_to_lines(*dy);
                // DFYP/1 specifies dx > 0 = RIGHT; CoreGraphics wheel2 positive = LEFT.
                let h_lines = -wheel_delta_to_lines(*dx);
                if v_lines == 0 && h_lines == 0 {
                    return Ok(());
                }
                let flags = self.current_flags();
                // SAFETY: source is valid/null; wheel event is posted and released.
                unsafe {
                    let event = CGEventCreateScrollWheelEvent2(
                        self.source.0,
                        K_CG_SCROLL_EVENT_UNIT_LINE,
                        2,
                        v_lines,
                        h_lines,
                        0,
                    );
                    if event.is_null() {
                        return Err(InputError::Send(
                            "CGEventCreateScrollWheelEvent2 返回空句柄".into(),
                        )
                        .into());
                    }
                    CGEventSetFlags(event, flags);
                    CGEventPost(K_CG_HID_EVENT_TAP, event);
                    CFRelease(event);
                }
                Ok(())
            }
            InputEvent::Text { text } => {
                if text.is_empty() {
                    return Ok(());
                }
                let units = utf16_units(text);
                // CGEventKeyboardSetUnicodeString recommends <= 20 UniChars per event.
                for chunk in units.chunks(20) {
                    for down in [true, false] {
                        // SAFETY: chunk pointer and length remain valid for the duration of
                        // CGEventKeyboardSetUnicodeString; event is posted and released cleanly.
                        unsafe {
                            let event = CGEventCreateKeyboardEvent(self.source.0, 0, down);
                            if event.is_null() {
                                return Err(InputError::Send(
                                    "CGEventCreateKeyboardEvent 返回空句柄".into(),
                                )
                                .into());
                            }
                            CGEventKeyboardSetUnicodeString(event, chunk.len(), chunk.as_ptr());
                            CGEventPost(K_CG_HID_EVENT_TAP, event);
                            CFRelease(event);
                        }
                    }
                }
                Ok(())
            }
        }
    }

    fn release_all(&mut self) {
        for ev in self.pressed.release_events() {
            if let Err(err) = self.inject(&ev) {
                debug!(error = %err, "释放 macOS 残留按下状态失败");
            }
        }
    }
}

/// 根据当前仍按下的 HID 修饰键构建 `CGEventFlags`。
fn modifier_flags_from_keys(keys: &[u16]) -> u64 {
    let mut flags = 0u64;
    for usage in keys {
        match *usage {
            0x39 => flags |= K_CG_EVENT_FLAG_MASK_ALPHA_SHIFT,
            0xE0 => flags |= K_CG_EVENT_FLAG_MASK_CONTROL | NX_DEVICELCTLKEYMASK,
            0xE1 => flags |= K_CG_EVENT_FLAG_MASK_SHIFT | NX_DEVICELSHIFTKEYMASK,
            0xE2 => flags |= K_CG_EVENT_FLAG_MASK_ALTERNATE | NX_DEVICELALTKEYMASK,
            0xE3 => flags |= K_CG_EVENT_FLAG_MASK_COMMAND | NX_DEVICELCMDKEYMASK,
            0xE4 => flags |= K_CG_EVENT_FLAG_MASK_CONTROL | NX_DEVICERCTLKEYMASK,
            0xE5 => flags |= K_CG_EVENT_FLAG_MASK_SHIFT | NX_DEVICERSHIFTKEYMASK,
            0xE6 => flags |= K_CG_EVENT_FLAG_MASK_ALTERNATE | NX_DEVICERALTKEYMASK,
            0xE7 => flags |= K_CG_EVENT_FLAG_MASK_COMMAND | NX_DEVICERCMDKEYMASK,
            _ => {}
        }
    }
    flags
}

/// 把 `[0..width) × [0..height)` 中的绝对坐标映射到 macOS 主显示器逻辑点矩形。
fn map_abs_to_rect(x: u32, y: u32, width: u32, height: u32, rect: CGRect) -> (f64, f64) {
    fn axis(value: u32, extent: u32, origin: f64, span: f64) -> f64 {
        let max_px = extent.saturating_sub(1);
        if max_px == 0 || span <= 1.0 {
            return origin;
        }
        let ratio = f64::from(value.min(max_px)) / f64::from(max_px);
        origin + ratio * (span - 1.0)
    }
    (
        axis(x, width, rect.origin.x, rect.size.width),
        axis(y, height, rect.origin.y, rect.size.height),
    )
}

/// Windows `WHEEL_DELTA`（120 = 1 格 ≈ 3 行）→ macOS 行数（非零输入至少 ±1 行）。
fn wheel_delta_to_lines(delta: i16) -> i32 {
    if delta == 0 {
        return 0;
    }
    let scaled = (f64::from(delta) / 40.0).round() as i32;
    if scaled == 0 {
        i32::from(delta.signum())
    } else {
        scaled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_normalized_coordinates_into_logical_display_bounds() {
        let rect = CGRect {
            origin: CGPoint { x: 10.0, y: 20.0 },
            size: CGSize {
                width: 1440.0,
                height: 900.0,
            },
        };
        assert_eq!(map_abs_to_rect(0, 0, 65_536, 65_536, rect), (10.0, 20.0));
        assert_eq!(
            map_abs_to_rect(65_535, 65_535, 65_536, 65_536, rect),
            (1449.0, 919.0)
        );
        assert_eq!(map_abs_to_rect(100, 100, 1, 1, rect), (10.0, 20.0));
    }

    #[test]
    fn wheel_delta_converts_with_minimum_step() {
        assert_eq!(wheel_delta_to_lines(0), 0);
        assert_eq!(wheel_delta_to_lines(120), 3);
        assert_eq!(wheel_delta_to_lines(-120), -3);
        assert_eq!(wheel_delta_to_lines(10), 1);
        assert_eq!(wheel_delta_to_lines(-10), -1);
    }

    #[test]
    fn modifier_flags_reflect_pressed_keys() {
        let flags = modifier_flags_from_keys(&[0xE0, 0xE5, 0xE3, 0x04]);
        assert_ne!(flags & K_CG_EVENT_FLAG_MASK_CONTROL, 0);
        assert_ne!(flags & NX_DEVICELCTLKEYMASK, 0);
        assert_ne!(flags & K_CG_EVENT_FLAG_MASK_SHIFT, 0);
        assert_ne!(flags & NX_DEVICERSHIFTKEYMASK, 0);
        assert_ne!(flags & K_CG_EVENT_FLAG_MASK_COMMAND, 0);
        assert_ne!(flags & NX_DEVICELCMDKEYMASK, 0);
        assert_eq!(flags & K_CG_EVENT_FLAG_MASK_ALTERNATE, 0);
    }

    #[test]
    fn create_sink_reports_permission_or_succeeds_cleanly() {
        // Never requires TCC authorization to pass: checks either Ok or clear Unavailable error.
        match create_sink() {
            Ok(mut sink) => sink.release_all(),
            Err(err) => {
                let text = format!("{err:#}");
                assert!(
                    text.contains("辅助功能"),
                    "expected actionable Accessibility message, got: {text}"
                );
            }
        }
    }
}
