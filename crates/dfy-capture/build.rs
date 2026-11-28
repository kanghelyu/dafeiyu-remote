// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }

    println!("cargo:rerun-if-changed=src/macos_sck.m");

    cc::Build::new()
        .file("src/macos_sck.m")
        .flag("-fobjc-arc")
        .flag("-Wno-deprecated-declarations")
        .compile("dfy_capture_sck");

    for framework in [
        "ScreenCaptureKit",
        "CoreGraphics",
        "CoreMedia",
        "CoreVideo",
        "CoreFoundation",
        "Foundation",
        "IOSurface",
    ] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
}
