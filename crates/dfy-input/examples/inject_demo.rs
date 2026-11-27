// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 打印 HID 映射表；只有显式传 `--type "<文本>"` 才真实注入（默认不动键鼠）。
//!
//! ```text
//! cargo run -p dfy-input --example inject_demo
//! cargo run -p dfy-input --example inject_demo -- --type "hello"
//! ```

use dfy_input::{
    create_sink, decide_shortcut, HidKeymap, InputEvent, ShortcutContext, ShortcutTable,
};

fn main() -> anyhow::Result<()> {
    print_keymap();
    print_shortcut_demo();

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--type" => {
                let text = args.next().unwrap_or_default();
                inject_text(&text)?;
            }
            "--help" | "-h" => {
                println!("用法：inject_demo [--type <文本>]");
                println!("  默认只打印映射表与快捷键决策示例，不注入任何事件。");
                println!("  --type 会把给定文本以 Unicode 方式注入当前焦点窗口。");
                return Ok(());
            }
            other => eprintln!("忽略未知参数：{other}"),
        }
    }
    Ok(())
}

fn print_keymap() {
    println!("HID Usage 映射表（usage / Windows VK / set-1 scancode）");
    println!("  usage   vk      scancode");
    for entry in HidKeymap::entries() {
        let usage = entry.usage;
        let vk = entry.vk;
        let scancode = entry.scancode;
        println!("  0x{usage:04X}  0x{vk:04X}  0x{scancode:04X}");
    }
    println!("共 {} 个键位映射", HidKeymap::entries().len());
}

fn print_shortcut_demo() {
    let table = ShortcutTable {
        controller_only: vec![0x14],      // Q
        local_function: vec![0xE3, 0x2B], // LGUI、Tab
        system_local: vec![0x29, 0x46],   // Esc、PrintScreen
    };
    let ctx = ShortcutContext {
        modifiers: 0,
        injected_by_us: false,
        controller_only: true,
    };
    println!("快捷键四态示例（controller_only = true）：");
    for usage in [0x14u16, 0xE3, 0x29, 0x2B, 0x04] {
        let decision = decide_shortcut(&table, usage, &ctx);
        println!("  usage 0x{usage:04X} -> {decision:?}");
    }
}

fn inject_text(text: &str) -> anyhow::Result<()> {
    if text.is_empty() {
        println!("--type 传入空文本，跳过注入。");
        return Ok(());
    }
    let mut sink = create_sink()?;
    sink.inject(&InputEvent::Text {
        text: text.to_owned(),
    })?;
    sink.release_all();
    let chars = text.chars().count();
    let units = dfy_input::utf16_units(text).len();
    println!("已注入 {chars} 个字符（{units} 个 UTF-16 码元）。");
    Ok(())
}
