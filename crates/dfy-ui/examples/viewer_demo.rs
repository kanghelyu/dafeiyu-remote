// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! `viewer_demo`：手动检查 viewer 窗口（显示/缩放/letterbox/快捷键）的小工具，
//! 同时演示 dfy-ui 的两种接入方式。
//!
//! 用法（`--` 之后是示例自己的参数）：
//!
//! - `cargo run -p dfy-ui --example viewer_demo`
//!   常驻模式：`spawn_viewer` + 30fps 合成帧推 3 秒，主线程打印窗口事件。
//! - `cargo run -p dfy-ui --example viewer_demo -- --run`
//!   阻塞模式：`run_viewer_with` + [`FrameIter`] 惰性产帧，演示 CLI `view` 子命令的集成形态；
//!   帧来源结束后自动关窗退出。
//! - `cargo run -p dfy-ui --example viewer_demo -- --smoke`
//!   无头 CI 冒烟：只确认窗口能在 5 秒内创建，然后退出（退出码为 0）。
//! - `--fullscreen` 启动即全屏；`--scale 1.5` 改首帧定窗的缩放倍数；
//!   `--seconds 30` 改演示时长（默认 3 秒，方便手动抓图/肉眼验收）。
//!
//! 窗口内快捷键：`F11` 全屏切换、`Esc` 退出、`Ctrl+Alt+Shift+Q` 打印 ToggleControl 事件。

use std::time::{Duration, Instant};

use dfy_ui::{
    run_viewer_with, spawn_viewer, Frame, FrameIter, ViewerConfig, ViewerControl, ViewerEvent,
};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const FPS: u64 = 30;
const FRAME_INTERVAL: Duration = Duration::from_millis(1000 / FPS);
const DEMO_FRAMES: u32 = 90;

/// 默认演示时长（秒）。`--seconds` 可覆盖；帧数 = 时长 × FPS。
const DEMO_SECONDS: f64 = DEMO_FRAMES as f64 / FPS as f64;

/// 示例参数。
#[derive(Debug, Default)]
struct Options {
    smoke: bool,
    run: bool,
    fullscreen: bool,
    scale: Option<f64>,
    seconds: Option<f64>,
}

fn main() -> anyhow::Result<()> {
    init_tracing();
    let options = parse_args();

    let base = ViewerConfig::new("大肥鱼远程 · viewer demo")
        .with_initial_size(WIDTH, HEIGHT)
        .with_fullscreen(options.fullscreen)
        .with_stats(true)
        // 演示「首帧定窗」：窗口会按首帧尺寸 × scale 调整一次；--scale 可覆盖。
        .with_scale(Some(options.scale.unwrap_or(1.0)));

    let frames = frame_budget(options.seconds);

    if options.smoke {
        run_smoke(base)
    } else if options.run {
        run_blocking(base, frames)
    } else {
        run_spawned(base, frames)
    }
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();
}

fn parse_args() -> Options {
    let mut options = Options::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--smoke" => options.smoke = true,
            "--run" => options.run = true,
            "--fullscreen" => options.fullscreen = true,
            "--scale" => options.scale = parse_scale(args.next()),
            "--seconds" => options.seconds = parse_scale(args.next()),
            other => eprintln!("忽略未知参数 {other}"),
        }
    }
    options
}

/// 把 `--seconds` 换算成帧数（至少 1 帧）。
fn frame_budget(seconds: Option<f64>) -> u32 {
    let seconds = seconds.unwrap_or(DEMO_SECONDS).clamp(0.1, 600.0);
    let frames = (seconds * FPS as f64).round() as u32;
    frames.max(1)
}

fn parse_scale(value: Option<String>) -> Option<f64> {
    value
        .and_then(|text| text.parse::<f64>().ok())
        .filter(|scale| scale.is_finite() && *scale > 0.0)
}

/// 常驻模式：UI 在独立线程，主线程推帧并收事件。
fn run_spawned(config: ViewerConfig, frames: u32) -> anyhow::Result<()> {
    let mut viewer = spawn_viewer(config)?;
    wait_for_window(&viewer)?;

    let started = Instant::now();
    let mut next_frame_at = Instant::now();
    let mut pushed = 0u32;
    let timeout = FRAME_INTERVAL * frames + Duration::from_secs(10);
    while pushed < frames && started.elapsed() < timeout {
        while let Some(event) = viewer.try_recv() {
            println!("VIEWER EVENT {event:?}");
            if event == ViewerEvent::Closed {
                return Ok(());
            }
        }
        if Instant::now() >= next_frame_at {
            viewer
                .present
                .present(WIDTH, HEIGHT, build_frame(WIDTH, HEIGHT, pushed));
            pushed += 1;
            next_frame_at = Instant::now() + FRAME_INTERVAL;
        }
        std::thread::sleep(Duration::from_millis(2));
    }

    // 让最后一帧上屏，再收一轮事件后关闭。
    std::thread::sleep(Duration::from_millis(100));
    while let Some(event) = viewer.try_recv() {
        println!("VIEWER EVENT {event:?}");
    }
    viewer.close();
    std::thread::sleep(Duration::from_millis(200));
    println!("DEMO_DONE frames={pushed}");
    Ok(())
}

/// 阻塞模式（CLI `view` 子命令的集成形态）：帧来源实现 [`dfy_ui::FrameSource`]，
/// UI 在调用线程跑，事件走回调。
fn run_blocking(config: ViewerConfig, frames: u32) -> anyhow::Result<()> {
    let frames = (0..frames).map(|index| {
        Frame::new(WIDTH, HEIGHT, build_frame(WIDTH, HEIGHT, index)).expect("演示帧尺寸固定且合法")
    });
    let source = FrameIter::new(frames).with_interval(FRAME_INTERVAL);

    run_viewer_with(config, source, |event| match event {
        ViewerEvent::Closed => {
            println!("VIEWER EVENT Closed");
            ViewerControl::Close
        }
        ViewerEvent::Key { usage, down, .. } => {
            println!("KEY usage=0x{usage:02X} down={down}");
            ViewerControl::Continue
        }
        other => {
            let _ = other;
            ViewerControl::Continue
        }
    })?;
    println!("DEMO_DONE");
    Ok(())
}

/// 无头 CI 冒烟：确认窗口真的起来了，然后立刻退出。
fn run_smoke(config: ViewerConfig) -> anyhow::Result<()> {
    let viewer = spawn_viewer(config)?;
    wait_for_window(&viewer)?;
    std::thread::sleep(Duration::from_millis(500));
    viewer.close();
    std::thread::sleep(Duration::from_millis(100));
    println!("SMOKE_OK");
    Ok(())
}

fn wait_for_window(viewer: &dfy_ui::ViewerHandle) -> anyhow::Result<()> {
    let started = Instant::now();
    while !viewer.present.is_window_ready() {
        if started.elapsed() > Duration::from_secs(5) {
            anyhow::bail!("窗口 5 秒内未就绪（无显示环境？）");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    println!("WINDOW_READY=1");
    Ok(())
}

/// 生成一帧 BGRA 渐变 + 棋盘格测试图；`index` 移动色相以便肉眼确认「帧在更新」。
fn build_frame(width: u32, height: u32, index: u32) -> Vec<u8> {
    let mut bgra = vec![0u8; width as usize * height as usize * 4];
    let hue = (index.wrapping_mul(4) & 0xFF) as u8;
    for y in 0..height {
        for x in 0..width {
            let offset = ((y * width + x) * 4) as usize;
            let light_square = ((x / 64) + (y / 64)) % 2 == 0;
            let (blue, green, red) = if light_square {
                (
                    hue.wrapping_add((x % 256) as u8),
                    (y % 256) as u8,
                    hue.wrapping_add(64),
                )
            } else {
                (
                    hue.wrapping_add(64),
                    hue.wrapping_add((x % 256) as u8),
                    (y % 256) as u8,
                )
            };
            bgra[offset] = blue;
            bgra[offset + 1] = green;
            bgra[offset + 2] = red;
            bgra[offset + 3] = 0xFF;
        }
    }
    bgra
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_frame_has_expected_length_alpha_and_checkerboard() {
        let (width, height) = (64u32, 32u32);
        let bgra = build_frame(width, height, 0);
        assert_eq!(bgra.len(), (width * height * 4) as usize);
        assert!(bgra.iter().skip(3).step_by(4).all(|alpha| *alpha == 0xFF));

        // 相邻两个 64px 棋盘格的颜色应不同（证明棋盘画出来了）。
        let row = 4usize;
        let left_at = (row * width as usize + 4) * 4;
        let right_at = (row * width as usize + 68) * 4;
        assert_ne!(&bgra[left_at..left_at + 4], &bgra[right_at..right_at + 4]);
    }

    #[test]
    fn frame_budget_follows_seconds_and_keeps_a_floor_of_one() {
        assert_eq!(frame_budget(None), 90);
        assert_eq!(frame_budget(Some(2.0)), 60);
        assert_eq!(frame_budget(Some(0.0)), 3);
        assert_eq!(frame_budget(Some(-5.0)), 3);
        assert_eq!(frame_budget(Some(10_000.0)), 18_000);
    }

    #[test]
    fn build_frame_changes_with_index() {
        let first = build_frame(8, 8, 0);
        let later = build_frame(8, 8, 8);
        assert_ne!(first, later);
        // 生成器输出的永远是「合法帧」。
        Frame::new(8, 8, first).expect("演示帧应通过校验");
    }
}
