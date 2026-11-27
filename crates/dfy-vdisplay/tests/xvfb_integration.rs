// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (C) 2025 Dafeiyu Remote Project

#![cfg(target_os = "linux")]

//! Opt-in Linux integration coverage. Run with:
//! `cargo test -p dfy-vdisplay --target x86_64-unknown-linux-gnu --test xvfb_integration -- --ignored`
//!
//! Requires Xvfb, xdpyinfo and xrandr on PATH. Only displays allocated by this test are
//! touched; existing displays (including a lab's physical/virtual desktop) are not used.

use std::process::Command;
use std::time::{Duration, Instant};

use dfy_vdisplay::{
    create_backend, linux_xvfb::XvfbBackend, VirtualDisplayBackend, VirtualDisplaySpec,
};

fn xdpyinfo(display: &str) -> String {
    let output = Command::new("xdpyinfo")
        .arg("-display")
        .arg(display)
        .env("DISPLAY", display)
        .output()
        .expect("start xdpyinfo");
    assert!(
        output.status.success(),
        "xdpyinfo {display} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn assert_geometry(display: &str, width: u32, height: u32) {
    let text = xdpyinfo(display);
    let dimensions = text
        .lines()
        .find(|line| line.trim_start().starts_with("dimensions:"))
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or_else(|| panic!("xdpyinfo {display} returned no dimensions"));
    assert_eq!(dimensions, format!("{width}x{height}"));
}

#[test]
#[ignore = "requires a Linux host with Xvfb, xdpyinfo and xrandr"]
fn creates_independent_displays_resizes_recovers_and_destroys() {
    let backend = create_backend().expect("create Linux backend");
    assert!(
        backend.is_available(),
        "install xvfb, x11-utils and x11-xserver-utils"
    );
    let first_spec = VirtualDisplaySpec::new(320, 240, 60, 96, "dfy-xvfb-integration-1");
    let second_spec = VirtualDisplaySpec::new(400, 300, 60, 96, "dfy-xvfb-integration-2");
    let first = backend
        .create(&first_spec)
        .expect("create first Xvfb display");
    let second = backend
        .create(&second_spec)
        .expect("create second Xvfb display");
    assert_ne!(first, second, "ids must be unique");

    // A separately constructed backend shares the per-user registry, demonstrating that
    // displays are independent of a particular control-plane instance.
    let observer = XvfbBackend::new().expect("create observer backend");
    let observer: Box<dyn VirtualDisplayBackend> = Box::new(observer);
    let listed = observer.list().expect("list through independent backend");
    assert!(listed.iter().any(|info| info.id == first && info.attached));
    assert!(listed.iter().any(|info| info.id == second && info.attached));

    // Public endpoints are available on the concrete backend without changing the trait.
    let control = XvfbBackend::new().expect("create endpoint backend");
    let first_display = control.display_name(first).expect("first display endpoint");
    let second_display = control
        .display_name(second)
        .expect("second display endpoint");
    assert_ne!(first_display, second_display);
    assert_geometry(&first_display, 320, 240);
    assert_geometry(&second_display, 400, 300);

    // A shrink must be performed live by xrandr --fb on the measured lab Xvfb.
    let pid_before_shrink = control.server_pid(first).unwrap();
    let shrunk = VirtualDisplaySpec::new(256, 192, 60, 96, "ignored-on-update");
    backend
        .update(first, &shrunk)
        .expect("live framebuffer shrink");
    assert_geometry(&first_display, 256, 192);
    assert_eq!(control.server_pid(first).unwrap(), pid_before_shrink);
    assert_geometry(&second_display, 400, 300);

    // Xvfb commonly rejects enlargement beyond its initial screen. update() must verify
    // that case and transparently recreate only this desktop under the same public id.
    let resized = VirtualDisplaySpec::new(640, 480, 60, 96, "ignored-on-update");
    backend.update(first, &resized).expect("verified resize");
    assert_geometry(&first_display, 640, 480);
    assert_geometry(&second_display, 400, 300);
    let first_info = backend
        .list()
        .unwrap()
        .into_iter()
        .find(|info| info.id == first)
        .unwrap();
    assert_eq!(first_info.spec.name_hint, first_spec.name_hint);

    let unchanged = backend
        .list()
        .unwrap()
        .into_iter()
        .find(|info| info.id == first)
        .unwrap()
        .spec;
    let refresh_change = VirtualDisplaySpec {
        refresh_hz: 75,
        ..unchanged
    };
    let error = backend.update(first, &refresh_change).unwrap_err();
    assert!(error.to_string().contains("unsupported"), "{error}");

    // Kill only the PID that the backend has verified as its own managed server.
    let old_pid = control.server_pid(first).unwrap();
    Command::new("kill")
        .arg("-KILL")
        .arg(old_pid.to_string())
        .status()
        .expect("send SIGKILL to integration Xvfb")
        .success()
        .then_some(())
        .expect("kill integration Xvfb");
    let deadline = Instant::now() + Duration::from_secs(3);
    while control.server_pid(first).ok() == Some(old_pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }

    // list() is the watchdog: it must recreate the dead server under the same public id.
    let recovered = backend
        .list()
        .expect("health-check display list")
        .into_iter()
        .find(|info| info.id == first)
        .expect("recreated display id remains stable");
    assert!(recovered.attached);
    assert_ne!(control.server_pid(first).unwrap(), old_pid);
    let recreated_display = control.display_name(first).unwrap();
    assert_geometry(
        &recreated_display,
        recovered.spec.width,
        recovered.spec.height,
    );
    assert_geometry(&second_display, 400, 300);

    backend.destroy(second).expect("destroy second display");
    assert!(backend.destroy(second).is_err(), "double destroy must fail");
    assert!(!second_display_is_connectable(&second_display));
    assert_geometry(
        &recreated_display,
        recovered.spec.width,
        recovered.spec.height,
    );
    backend.destroy(first).expect("destroy first display");
    assert!(!second_display_is_connectable(&recreated_display));
}

fn second_display_is_connectable(display: &str) -> bool {
    Command::new("xdpyinfo")
        .arg("-display")
        .arg(display)
        .env("DISPLAY", display)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}
