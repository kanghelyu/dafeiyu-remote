// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! Opt-in Linux X11 integration test. NEVER run against an interactive desktop.
//!
//! Run only inside an isolated disposable Xvfb/Xorg display, for example:
//! ```text
//! Xvfb :99 -screen 0 1280x720x24 -nolisten tcp
//! DISPLAY=:99 XDG_SESSION_TYPE=x11 DFY_INPUT_X11_TEST=1 cargo test -p dfy-input --test x11_inject -- --ignored --test-threads=1
//! ```
//! An independent X11 connection observes events, pointer position and server key state.
//! Default `cargo test` never runs this test or connects to DISPLAY.

#![cfg(target_os = "linux")]

use dfy_input::{create_sink, InputEvent, KeyState, MouseButton};
use x11rb::connection::Connection;
use x11rb::protocol::xkb::{ConnectionExt as _, NameDetail, ID};
use x11rb::protocol::xproto::{
    ConnectionExt as _, CreateWindowAux, EventMask, InputFocus, KeyButMask, WindowClass,
};
use x11rb::protocol::Event;
use x11rb::{COPY_DEPTH_FROM_PARENT, CURRENT_TIME};

#[test]
#[ignore = "injects real X11 input; requires an explicitly opted-in disposable display"]
fn isolated_x11_observer_confirms_input_and_disconnect_releases() -> anyhow::Result<()> {
    anyhow::ensure!(
        std::env::var("DFY_INPUT_X11_TEST").as_deref() == Ok("1"),
        "set DFY_INPUT_X11_TEST=1 only with a disposable isolated X11 display"
    );
    let (observer, screen_index) = x11rb::connect(None)?;
    let screen = &observer.setup().roots[screen_index];
    anyhow::ensure!(
        screen.width_in_pixels >= 640 && screen.height_in_pixels >= 360,
        "test requires at least 640x360"
    );
    let root = screen.root;
    let (width, height) = (screen.width_in_pixels, screen.height_in_pixels);
    let window = observer.generate_id()?;
    observer
        .create_window(
            COPY_DEPTH_FROM_PARENT,
            window,
            root,
            0,
            0,
            width,
            height,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new().override_redirect(1).event_mask(
                EventMask::KEY_PRESS
                    | EventMask::KEY_RELEASE
                    | EventMask::BUTTON_PRESS
                    | EventMask::BUTTON_RELEASE
                    | EventMask::POINTER_MOTION,
            ),
        )?
        .check()?;
    observer.map_window(window)?.check()?;
    observer
        .set_input_focus(InputFocus::PARENT, window, CURRENT_TIME)?
        .check()?;
    let xkb = observer.xkb_use_extension(1, 0)?.reply()?;
    anyhow::ensure!(xkb.supported, "XKB must be available");
    let names = observer
        .xkb_get_names(ID::USE_CORE_KBD.into(), NameDetail::KEY_NAMES)?
        .reply()?;
    let codes = names
        .value_list
        .key_names
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no XKB names"))?;
    let code_for = |name: &[u8; 4]| -> anyhow::Result<u8> {
        codes
            .iter()
            .position(|key| key.name == *name)
            .map(|offset| (usize::from(names.first_key) + offset) as u8)
            .ok_or_else(|| anyhow::anyhow!("missing physical key {name:?}"))
    };
    let a = code_for(b"AC01")?;
    let ctrl = code_for(b"LCTL")?;
    let pressed = |code: u8| -> anyhow::Result<bool> {
        let keys = observer.query_keymap()?.reply()?.keys;
        Ok(keys[usize::from(code / 8)] & (1 << (code % 8)) != 0)
    };

    let mut sink = create_sink()?;
    sink.inject(&InputEvent::MouseMoveAbs {
        x: 639,
        y: 359,
        width: 640,
        height: 360,
    })?;
    let pointer = observer.query_pointer(root)?.reply()?;
    assert_eq!(
        (pointer.root_x, pointer.root_y),
        ((width - 1) as i16, (height - 1) as i16)
    );
    sink.inject(&InputEvent::MouseMoveAbs {
        x: 100,
        y: 100,
        width: u32::from(width),
        height: u32::from(height),
    })?;
    sink.inject(&InputEvent::MouseMoveRel { dx: -7, dy: 5 })?;
    let pointer = observer.query_pointer(root)?.reply()?;
    assert_eq!((pointer.root_x, pointer.root_y), (93, 105));

    for usage in [0xe0, 0x04] {
        sink.inject(&InputEvent::Key {
            usage,
            state: KeyState::Down,
            modifiers: 0,
        })?;
    }
    assert!(
        pressed(ctrl)? && pressed(a)?,
        "independent QueryKeymap must see held keys"
    );
    sink.release_all();
    assert!(
        !pressed(ctrl)? && !pressed(a)?,
        "release_all must clear server key state"
    );

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
        })?;
        sink.inject(&InputEvent::MouseButton {
            button,
            state: KeyState::Up,
        })?;
    }
    sink.inject(&InputEvent::Wheel { dx: 60, dy: 30 })?;
    sink.inject(&InputEvent::Wheel { dx: 60, dy: 90 })?;
    sink.inject(&InputEvent::Wheel { dx: -120, dy: -120 })?;

    assert!(sink
        .inject(&InputEvent::Text {
            text: "中😀".into()
        })
        .is_err());
    assert!(sink
        .inject(&InputEvent::Key {
            usage: 0xffff,
            state: KeyState::Down,
            modifiers: 0
        })
        .is_err());

    sink.inject(&InputEvent::Key {
        usage: 0xe0,
        state: KeyState::Down,
        modifiers: 0,
    })?;
    sink.inject(&InputEvent::MouseButton {
        button: MouseButton::Left,
        state: KeyState::Down,
    })?;
    assert!(pressed(ctrl)?);
    assert!(observer
        .query_pointer(root)?
        .reply()?
        .mask
        .contains(KeyButMask::BUTTON1));
    drop(sink);
    assert!(!pressed(ctrl)?, "Drop must release held modifiers");
    assert!(
        !observer
            .query_pointer(root)?
            .reply()?
            .mask
            .contains(KeyButMask::BUTTON1),
        "Drop must release held pointer buttons"
    );

    // A round trip on the observer ensures the server has delivered the preceding events.
    observer.get_input_focus()?.reply()?;
    let mut events = Vec::new();
    while let Some(event) = observer.poll_for_event()? {
        events.push(event);
    }
    assert!(events
        .iter()
        .any(|ev| matches!(ev, Event::KeyPress(key) if key.detail == a)));
    assert!(events
        .iter()
        .any(|ev| matches!(ev, Event::KeyRelease(key) if key.detail == a)));
    for button in [1, 2, 3, 4, 5, 6, 7, 8, 9] {
        assert!(
            events
                .iter()
                .any(|ev| matches!(ev, Event::ButtonPress(key) if key.detail == button)),
            "observer did not receive button {button} press"
        );
        assert!(
            events
                .iter()
                .any(|ev| matches!(ev, Event::ButtonRelease(key) if key.detail == button)),
            "observer did not receive button {button} release"
        );
    }
    observer.destroy_window(window)?.check()?;
    Ok(())
}
