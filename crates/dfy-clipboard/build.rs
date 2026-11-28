// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

fn main() {
    println!("cargo:rerun-if-changed=src/macos_pb.m");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        #[cfg(target_os = "macos")]
        {
            cc::Build::new()
                .file("src/macos_pb.m")
                .flag("-fobjc-arc")
                .compile("dfy_clipboard_pb");
            println!("cargo:rustc-link-lib=framework=AppKit");
            println!("cargo:rustc-link-lib=framework=Foundation");
        }
    }
}
