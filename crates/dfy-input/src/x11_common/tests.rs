// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 无显示连接的测试，使用真实后端相同的映射和状态机。

use super::*;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct MockState {
    calls: Vec<Vec<X11Event>>,
    fail_next: bool,
    keycode: u8,
    geometry: (u32, u32),
}

struct MockDriver(Arc<Mutex<MockState>>);

impl X11Driver for MockDriver {
    fn keycode(&mut self, usage: u16) -> anyhow::Result<u8> {
        physical_key_name(usage)
            .ok_or_else(|| InputError::UnsupportedEvent("unknown HID".into()))?;
        Ok(self.0.lock().unwrap().keycode)
    }

    fn geometry(&mut self) -> anyhow::Result<(u32, u32)> {
        Ok(self.0.lock().unwrap().geometry)
    }

    fn send(&mut self, events: &[X11Event]) -> anyhow::Result<()> {
        let mut state = self.0.lock().unwrap();
        state.calls.push(events.to_vec());
        if std::mem::take(&mut state.fail_next) {
            return Err(InputError::Send("simulated connection/request failure".into()).into());
        }
        Ok(())
    }
}

fn mock_sink() -> (X11Sink<MockDriver>, Arc<Mutex<MockState>>) {
    let state = Arc::new(Mutex::new(MockState {
        keycode: 38,
        geometry: (1920, 1080),
        ..MockState::default()
    }));
    (X11Sink::new(MockDriver(Arc::clone(&state))), state)
}

fn key(usage: u16, state: KeyState) -> InputEvent {
    // 位图是诊断信息，不能因此额外注入 Ctrl/Alt/Shift。
    InputEvent::Key {
        usage,
        state,
        modifiers: 0xff,
    }
}

#[test]
fn all_existing_keyboard_usages_have_physical_xkb_names() {
    for entry in crate::HidKeymap::entries() {
        assert!(
            physical_key_name(entry.usage).is_some(),
            "unmapped {:#x}",
            entry.usage
        );
    }
    assert_eq!(physical_key_name(0x04), Some(*b"AC01"));
    assert_eq!(physical_key_name(0x1c), Some(*b"AD06")); // Y 的物理位置，不按目标布局查 keysym
    assert_eq!(physical_key_name(0x64), Some(*b"LSGT"));
    assert_eq!(physical_key_name(0xe6), Some(*b"RALT"));
    assert_eq!(physical_key_name(0xffff), None);
}

#[test]
fn physical_keycodes_come_from_server_names_and_aliases() {
    let map =
        PhysicalKeymap::new(90, &[*b"ALGR", *b"LCTL", *b"AC01"], &[(*b"ALGR", *b"RALT")]).unwrap();
    assert_eq!(map.keycode(0xe6).unwrap(), 90);
    assert_eq!(map.keycode(0xe0).unwrap(), 91);
    assert_eq!(map.keycode(0x04).unwrap(), 92); // 不是常见 evdev/Xorg 的 38
    assert!(matches!(
        map.keycode(0x05).unwrap_err().downcast_ref::<InputError>(),
        Some(InputError::UnsupportedEvent(_))
    ));
    assert!(map.keycode(0xffff).is_err());
    assert!(PhysicalKeymap::new(7, &[*b"AC01"], &[]).is_err());
    assert!(PhysicalKeymap::new(255, &[*b"AC01", *b"LCTL"], &[]).is_err());
    assert!(PhysicalKeymap::new(8, &[*b"AC01", *b"AC01"], &[]).is_err());
}

#[test]
fn wayland_is_rejected_even_when_xwayland_display_exists() {
    for (session, wayland) in [
        (Some("wayland"), None),
        (Some("WAYLAND"), Some("wayland-0")),
        (None, Some("wayland-1")),
    ] {
        let error = require_x11_session(session, wayland, Some(":0")).unwrap_err();
        assert!(matches!(
            error.downcast_ref::<InputError>(),
            Some(InputError::UnsupportedSession(_))
        ));
    }
    assert!(require_x11_session(Some("x11"), None, None).is_err());
    assert!(require_x11_session(Some("x11"), None, Some("")).is_err());
    assert!(require_x11_session(None, None, Some(":99")).is_ok()); // 显式 X11/SSH/Xvfb DISPLAY
    assert!(require_x11_session(Some("x11"), Some("stale-value"), Some(":0")).is_ok());
}

#[test]
fn key_up_and_disconnect_use_the_original_keycode() {
    let (mut sink, state) = mock_sink();
    sink.inject(&key(0x04, KeyState::Down)).unwrap();
    sink.inject(&key(0x04, KeyState::Down)).unwrap();
    assert_eq!(sink.keys.len(), 1);
    state.lock().unwrap().keycode = 92;
    sink.inject(&key(0x04, KeyState::Up)).unwrap();
    assert!(sink.keys.is_empty());
    assert_eq!(
        state.lock().unwrap().calls,
        vec![
            vec![X11Event::Key {
                code: 38,
                state: KeyState::Down
            }],
            vec![X11Event::Key {
                code: 38,
                state: KeyState::Down
            }],
            vec![X11Event::Key {
                code: 38,
                state: KeyState::Up
            }],
        ]
    );
    sink.inject(&key(0xe1, KeyState::Down)).unwrap();
    state.lock().unwrap().keycode = 50;
    sink.release_all();
    assert_eq!(
        state.lock().unwrap().calls.last().unwrap(),
        &vec![X11Event::Key {
            code: 92,
            state: KeyState::Up
        },]
    );
    let calls = state.lock().unwrap().calls.len();
    sink.release_all();
    assert_eq!(state.lock().unwrap().calls.len(), calls);
}

#[test]
fn unknown_keys_and_text_fail_without_sending_or_tracking_input() {
    let (mut sink, state) = mock_sink();
    assert!(sink.inject(&key(0xffff, KeyState::Down)).is_err());
    assert!(sink
        .inject(&InputEvent::Text {
            text: "中😀".into()
        })
        .is_err());
    sink.inject(&InputEvent::Text {
        text: String::new(),
    })
    .unwrap();
    assert!(sink.keys.is_empty());
    assert!(state.lock().unwrap().calls.is_empty());
}

#[test]
fn failed_down_and_up_remain_releasable_and_drop_retries() {
    let (mut sink, state) = mock_sink();
    state.lock().unwrap().fail_next = true;
    assert!(sink.inject(&key(0xe0, KeyState::Down)).is_err());
    assert_eq!(sink.keys, vec![(0xe0, 38)]);
    state.lock().unwrap().fail_next = true;
    sink.release_all();
    assert_eq!(sink.keys, vec![(0xe0, 38)]); // 没有丢掉失败的释放
    drop(sink);
    assert_eq!(
        state.lock().unwrap().calls,
        vec![
            vec![X11Event::Key {
                code: 38,
                state: KeyState::Down
            }],
            vec![X11Event::Key {
                code: 38,
                state: KeyState::Up
            }],
            vec![X11Event::Key {
                code: 38,
                state: KeyState::Up
            }],
        ]
    );
}

#[test]
fn shared_physical_key_is_released_by_the_last_hid_holder() {
    let (mut sink, state) = mock_sink();
    sink.inject(&key(0x31, KeyState::Down)).unwrap();
    sink.inject(&key(0x32, KeyState::Down)).unwrap();
    sink.inject(&key(0x31, KeyState::Up)).unwrap();
    assert_eq!(state.lock().unwrap().calls.len(), 1);
    sink.inject(&key(0x32, KeyState::Up)).unwrap();
    assert_eq!(state.lock().unwrap().calls.len(), 2);
    assert!(sink.keys.is_empty());
    sink.inject(&key(0x31, KeyState::Down)).unwrap();
    sink.inject(&key(0x32, KeyState::Down)).unwrap();
    let before = state.lock().unwrap().calls.len();
    sink.release_all();
    assert_eq!(state.lock().unwrap().calls.len(), before + 1);
    assert!(sink.keys.is_empty());
}

#[test]
fn all_five_mouse_buttons_are_released_and_failed_release_is_retried() {
    let (mut sink, state) = mock_sink();
    for button in [
        MouseButton::Left,
        MouseButton::Middle,
        MouseButton::Right,
        MouseButton::Back,
        MouseButton::Forward,
    ] {
        sink.inject(&InputEvent::MouseButton {
            button,
            state: KeyState::Down,
        })
        .unwrap();
    }
    state.lock().unwrap().fail_next = true;
    sink.release_all();
    assert_eq!(sink.buttons, vec![1]);
    sink.release_all();
    assert!(sink.buttons.is_empty());
    let log = state.lock().unwrap();
    let down_buttons: Vec<_> = log.calls[..5].iter().map(|v| v[0]).collect();
    assert_eq!(
        down_buttons,
        vec![
            X11Event::Button {
                button: 1,
                state: KeyState::Down
            },
            X11Event::Button {
                button: 2,
                state: KeyState::Down
            },
            X11Event::Button {
                button: 3,
                state: KeyState::Down
            },
            X11Event::Button {
                button: 8,
                state: KeyState::Down
            },
            X11Event::Button {
                button: 9,
                state: KeyState::Down
            },
        ]
    );
    assert_eq!(
        log.calls.last().unwrap(),
        &vec![X11Event::Button {
            button: 1,
            state: KeyState::Up
        }]
    );
}

#[test]
fn absolute_coordinates_follow_current_root_geometry_and_clamp() {
    let (mut sink, state) = mock_sink();
    sink.inject(&InputEvent::MouseMoveAbs {
        x: 639,
        y: 359,
        width: 640,
        height: 360,
    })
    .unwrap();
    state.lock().unwrap().geometry = (1280, 720);
    sink.inject(&InputEvent::MouseMoveAbs {
        x: u32::MAX,
        y: u32::MAX,
        width: 640,
        height: 360,
    })
    .unwrap();
    sink.inject(&InputEvent::MouseMoveAbs {
        x: 17,
        y: 29,
        width: 0,
        height: 0,
    })
    .unwrap();
    sink.inject(&InputEvent::MouseMoveRel {
        dx: i16::MIN,
        dy: i16::MAX,
    })
    .unwrap();
    assert_eq!(
        state.lock().unwrap().calls,
        vec![
            vec![X11Event::Motion {
                relative: false,
                x: 1919,
                y: 1079
            }],
            vec![X11Event::Motion {
                relative: false,
                x: 1279,
                y: 719
            }],
            vec![X11Event::Motion {
                relative: false,
                x: 17,
                y: 29
            }],
            vec![X11Event::Motion {
                relative: true,
                x: i16::MIN,
                y: i16::MAX
            }],
        ]
    );
    assert_eq!(scale_axis(u32::MAX, 1, 1920).unwrap(), 0);
    assert_eq!(scale_axis(u32::MAX, u32::MAX, 32768).unwrap(), 32767);
    assert!(scale_axis(0, 1, 0).is_err());
    assert!(scale_axis(0, 1, 32769).is_err());
}

#[test]
fn wheel_accumulates_subdetents_preserves_directions_and_resets_on_release() {
    let (mut sink, state) = mock_sink();
    sink.inject(&InputEvent::Wheel { dx: 60, dy: 30 }).unwrap();
    sink.inject(&InputEvent::Wheel { dx: 60, dy: 90 }).unwrap();
    sink.inject(&InputEvent::Wheel { dx: -240, dy: -120 })
        .unwrap();
    let buttons: Vec<_> = state
        .lock()
        .unwrap()
        .calls
        .iter()
        .map(|events| {
            assert_eq!(events.len(), 2);
            let X11Event::Button {
                button,
                state: KeyState::Down,
            } = events[0]
            else {
                panic!("expected wheel press")
            };
            assert_eq!(
                events[1],
                X11Event::Button {
                    button,
                    state: KeyState::Up
                }
            );
            button
        })
        .collect();
    assert_eq!(buttons, vec![4, 7, 5, 6, 6]);
    assert!(sink.buttons.is_empty());
    sink.inject(&InputEvent::Wheel { dx: 0, dy: 119 }).unwrap();
    sink.release_all();
    sink.inject(&InputEvent::Wheel { dx: 0, dy: 1 }).unwrap();
    assert_eq!(state.lock().unwrap().calls.len(), 5);
}

#[test]
fn partially_failed_wheel_batch_keeps_a_release_guard() {
    let (mut sink, state) = mock_sink();
    state.lock().unwrap().fail_next = true;
    assert!(sink.inject(&InputEvent::Wheel { dx: 0, dy: 120 }).is_err());
    assert_eq!(sink.buttons, vec![4]);
    sink.release_all();
    assert!(sink.buttons.is_empty());
    assert_eq!(
        state.lock().unwrap().calls.last().unwrap(),
        &vec![X11Event::Button {
            button: 4,
            state: KeyState::Up
        },]
    );
}
