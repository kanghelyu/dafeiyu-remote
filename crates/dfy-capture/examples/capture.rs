//! Smoke test / demo for `dfy-capture`.
//!
//! Usage:
//!
//! ```text
//! cargo run -p dfy-capture --example capture -- \
//!     --monitor 0 --frames 10 --out preview.png
//! ```
//!
//! Captures `--frames` desktop frames (roughly one per 16 ms whenever the
//! compositor presents a new image), prints a line per frame, writes the last
//! frame to a PNG (BGRA is converted to RGBA by hand) and prints a summary
//! (average inter-frame time, non-zero payload ratio, PNG path/size).
//!
//! Desktop Duplication only delivers a frame when the desktop actually changes.
//! On a completely idle desktop the loop would starve, so after a few
//! `WAIT_TIMEOUT`s the example nudges the mouse cursor a couple of pixels to
//! make the compositor present a new frame. That perturbation is reported in
//! the output; it does not change anything else on the desktop.
//!
//! Exit code: 0 on success, 1 on failure.

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(windows)]
fn run() -> anyhow::Result<()> {
    use std::io::Write as _;
    use std::time::{Duration, Instant};

    use dfy_capture::DxgiCapturer;

    let args = Args::parse(std::env::args().skip(1))?;
    if args.help {
        Args::print_help();
        return Ok(());
    }

    // ---- Monitors ---------------------------------------------------------
    let monitors = DxgiCapturer::monitors()?;
    println!("monitors ({}):", monitors.len());
    for m in &monitors {
        println!(
            "  [{}] id={} {:?} {}x{} at ({},{}) primary={} dpi={}",
            m.id, m.id, m.name, m.w, m.h, m.x, m.y, m.primary, m.dpi
        );
    }
    let target_monitor = monitors
        .get(args.monitor as usize)
        .ok_or_else(|| anyhow::anyhow!("monitor {} not found", args.monitor))?
        .clone();
    println!(
        "\ncapturing monitor {} ({}) {}x{} at ({},{})",
        target_monitor.id,
        target_monitor.name,
        target_monitor.w,
        target_monitor.h,
        target_monitor.x,
        target_monitor.y
    );

    // ---- Capture loop -----------------------------------------------------
    let frame_interval = Duration::from_millis(args.interval_ms as u64);
    let deadline = Instant::now() + Duration::from_millis(args.deadline_ms as u64);

    let mut capturer = DxgiCapturer::new(args.monitor)?;
    let mut last_frame = None;
    let mut frames: u64 = 0;
    let mut blank_frames: u64 = 0;
    let mut timeouts: u64 = 0;
    let mut consecutive_timeouts: u64 = 0;
    let mut nudges: u64 = 0;
    let mut intervals: Vec<Duration> = Vec::new();
    let mut previous_frame_at: Option<Instant> = None;

    while frames < args.frames as u64 {
        if Instant::now() >= deadline {
            break;
        }
        let started = Instant::now();
        let captured = capturer.capture(args.interval_ms)?;
        match captured {
            Some(frame) => {
                frames += 1;
                consecutive_timeouts = 0;
                if let Some(prev) = previous_frame_at {
                    intervals.push(started.duration_since(prev));
                }
                previous_frame_at = Some(started);
                let blank = is_blank(&frame);
                if blank {
                    blank_frames += 1;
                }
                println!(
                    "frame {:>2}: {}x{} stride={} dirty={} ts={}{}",
                    frames,
                    frame.width,
                    frame.height,
                    frame.stride,
                    frame.dirty.len(),
                    frame.timestamp_qpc,
                    if blank {
                        " (blank: no desktop image yet)"
                    } else {
                        ""
                    }
                );
                std::io::stdout().flush().ok();
                // Keep the first non-blank frame: a duplication object can hand
                // out one all-zero frame before the real desktop image arrives.
                if !blank {
                    last_frame = Some(frame);
                }
            }
            None => {
                timeouts += 1;
                consecutive_timeouts += 1;
                if args.nudge_cursor && consecutive_timeouts >= 5 && nudges < 500 {
                    nudges += 1;
                    nudge_cursor(nudges);
                    if nudges == 1 || nudges % 25 == 0 {
                        println!(
                            "  (no new frame from the compositor after {} timeouts; \
                             nudging the cursor to generate one)",
                            consecutive_timeouts
                        );
                    }
                    consecutive_timeouts = 0;
                }
            }
        }
        // Keep the requested cadence even when the compositor answers quickly.
        let elapsed = started.elapsed();
        if elapsed < frame_interval {
            std::thread::sleep(frame_interval - elapsed);
        }
    }

    // ---- Summary ----------------------------------------------------------
    println!("\n--- summary ---");
    println!("frames captured: {frames} (requested {})", args.frames);
    println!("blank frames: {blank_frames}");
    println!("timeouts: {timeouts}");
    println!(
        "capture rebuilds after ACCESS_LOST: {}",
        capturer.rebuilds()
    );
    if let Some(frame) = &last_frame {
        let min = intervals.iter().map(|d| *d).min();
        let max = intervals.iter().map(|d| *d).max();
        let avg = if intervals.is_empty() {
            Duration::from_millis(0)
        } else {
            intervals.iter().sum::<Duration>() / intervals.len() as u32
        };
        let packed = frame.to_packed();
        let nonzero = nonzero_ratio(&packed);
        println!(
            "frame size: {}x{} stride={} packed={} bytes",
            frame.width,
            frame.height,
            frame.stride,
            packed.len()
        );
        println!(
            "inter-frame time: avg={:.2?} min={:.2?} max={:.2?} (n={})",
            avg,
            min.unwrap_or_default(),
            max.unwrap_or_default(),
            intervals.len()
        );
        println!("non-zero payload ratio: {:.1}%", nonzero * 100.0);

        // ---- PNG ----------------------------------------------------------
        let rgba = to_rgba(frame);
        let parent = args.out.parent();
        if let Some(parent) = parent {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        write_png(&args.out, frame.width, frame.height, &rgba)?;
        let png_len = std::fs::metadata(&args.out)?.len();
        println!(
            "png: {} ({}x{}, {} bytes)",
            args.out.display(),
            frame.width,
            frame.height,
            png_len
        );

        if frame.width != target_monitor.w || frame.height != target_monitor.h {
            println!(
                "warning: captured {}x{} != monitor {}x{} (rotation={:?}?)",
                frame.width, frame.height, target_monitor.w, target_monitor.h, frame.format
            );
        }
        if png_len <= 10_000 {
            anyhow::bail!("preview PNG is only {png_len} bytes (<= 10 KB), capture looks empty");
        }
    } else {
        anyhow::bail!(
            "{frames} frames were captured but none contained a desktop image \
             (all blank); DXGI handed out placeholder frames only"
        );
    }

    if frames < args.frames as u64 {
        anyhow::bail!(
            "only {frames} of {} requested frames were captured before the deadline \
             (desktop may be idle; try a larger --deadline-ms)",
            args.frames
        );
    }
    println!(
        "OK: {frames} frames captured, preview written to {}",
        args.out.display()
    );
    Ok(())
}

#[cfg(windows)]
#[derive(Debug)]
struct Args {
    monitor: u32,
    frames: u32,
    interval_ms: u32,
    deadline_ms: u64,
    out: std::path::PathBuf,
    nudge_cursor: bool,
    help: bool,
}

#[cfg(windows)]
impl Args {
    fn parse<I: Iterator<Item = String>>(mut args: I) -> anyhow::Result<Self> {
        let mut parsed = Args {
            monitor: 0,
            frames: 10,
            interval_ms: 16,
            deadline_ms: 20_000,
            out: std::path::PathBuf::from("preview.png"),
            nudge_cursor: true,
            help: false,
        };
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--monitor" => parsed.monitor = take_value(&mut args, "--monitor")?.parse()?,
                "--frames" => parsed.frames = take_value(&mut args, "--frames")?.parse()?,
                "--interval-ms" => {
                    parsed.interval_ms = take_value(&mut args, "--interval-ms")?.parse()?
                }
                "--deadline-ms" => {
                    parsed.deadline_ms = take_value(&mut args, "--deadline-ms")?.parse()?
                }
                "--out" => parsed.out = std::path::PathBuf::from(take_value(&mut args, "--out")?),
                "--no-nudge" => parsed.nudge_cursor = false,
                "-h" | "--help" => parsed.help = true,
                other => anyhow::bail!("unknown argument {other:?} (try --help)"),
            }
        }
        if parsed.frames == 0 {
            anyhow::bail!("--frames must be at least 1");
        }
        Ok(parsed)
    }

    fn print_help() {
        println!(
            "dfy-capture example\n\
             \n\
             Usage: cargo run -p dfy-capture --example capture -- [OPTIONS]\n\
             \n\
             Options:\n\
             \x20 --monitor <N>       monitor index, as listed by monitors() (default 0)\n\
             \x20 --frames <N>        frames to capture (default 10)\n\
             \x20 --interval-ms <N>   minimum time between capture attempts (default 16)\n\
             \x20 --deadline-ms <N>   overall deadline for the capture loop (default 20000)\n\
             \x20 --out <PATH>        PNG output path (default preview.png)\n\
             \x20 --no-nudge          never move the cursor to induce desktop updates\n\
             \x20 --help              print this help"
        );
    }
}

#[cfg(windows)]
fn take_value<I: Iterator<Item = String>>(args: &mut I, flag: &str) -> anyhow::Result<String> {
    args.next()
        .ok_or_else(|| anyhow::anyhow!("{flag} requires a value"))
}

/// Cheap "is this the all-zero placeholder frame?" test.
///
/// A duplication object can return one black frame before DWM presents the
/// first real desktop image (observed with a virtual display adapter).
#[cfg(windows)]
fn is_blank(frame: &dfy_capture::CapturedFrame) -> bool {
    frame.data.iter().step_by(97).all(|&byte| byte == 0)
}

/// Picks a few thousand payload bytes and returns the ratio that is non-zero.
#[cfg(windows)]
fn nonzero_ratio(packed: &[u8]) -> f64 {
    if packed.is_empty() {
        return 0.0;
    }
    // Sample every 97th byte (prime) so the sample is spread over the image.
    let mut seen = 0usize;
    let mut nonzero = 0usize;
    for &byte in packed.iter().step_by(97) {
        seen += 1;
        if byte != 0 {
            nonzero += 1;
        }
    }
    if seen == 0 {
        0.0
    } else {
        nonzero as f64 / seen as f64
    }
}

/// Converts a captured frame to tightly packed RGBA (alpha forced opaque).
#[cfg(windows)]
fn to_rgba(frame: &dfy_capture::CapturedFrame) -> Vec<u8> {
    use dfy_capture::PixelFormat;

    let packed = frame.to_packed();
    let mut rgba = Vec::with_capacity(frame.width as usize * frame.height as usize * 4);
    match frame.format {
        PixelFormat::Bgra8 => {
            for px in packed.chunks_exact(4) {
                rgba.extend_from_slice(&[px[2], px[1], px[0], 0xFF]);
            }
        }
        PixelFormat::Rgba8 => {
            for px in packed.chunks_exact(4) {
                rgba.extend_from_slice(&[px[0], px[1], px[2], 0xFF]);
            }
        }
    }
    rgba
}

#[cfg(windows)]
fn write_png(path: &std::path::Path, width: u32, height: u32, rgba: &[u8]) -> anyhow::Result<()> {
    anyhow::ensure!(
        rgba.len() == width as usize * height as usize * 4,
        "RGBA buffer size mismatch ({} != {}x{}x4)",
        rgba.len(),
        width,
        height
    );
    let file = std::fs::File::create(path)
        .map_err(|e| anyhow::anyhow!("creating {} failed: {e}", path.display()))?;
    let writer = std::io::BufWriter::new(file);
    let mut encoder = png::Encoder::new(writer, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut png_writer = encoder.write_header()?;
    png_writer.write_image_data(rgba)?;
    png_writer.finish()?;
    Ok(())
}

/// Moves the cursor by a couple of pixels so the compositor presents a frame.
#[cfg(windows)]
fn nudge_cursor(step: u64) {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};
    // SAFETY: plain Win32 calls; the POINT out-parameter is a valid local.
    unsafe {
        let mut pos = POINT::default();
        if GetCursorPos(&mut pos).is_ok() {
            let dx = if step % 2 == 0 { 2 } else { -2 };
            let _ = SetCursorPos(pos.x + dx, pos.y);
        }
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("error: this example needs Windows (DXGI Desktop Duplication)");
    std::process::exit(1);
}
