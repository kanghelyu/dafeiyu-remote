// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 命令行演示：合成渐变帧 → H.264 编码 → 写出 Annex-B 码流文件。
//!
//! 用法：
//!
//! ```text
//! cargo run -p dfy-codec --example encode_demo -- --w 1920 --h 1080 --frames 30 \
//!     --out "D:\agent working\dafeiyu\.tmp\codec.h264"
//! ```
//!
//! 加 `--decode` 会把刚编码出来的码流再解回 NV12，验证解码器链路。

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Parser;
use dfy_codec::{
    available_encoders, create_decoder, create_encoder, Codec, DecodedFrame, EncoderConfig,
};

#[derive(Debug, Parser)]
#[command(
    name = "encode_demo",
    about = "dfy-codec 演示：合成画面 → Media Foundation H.264 编码"
)]
struct Args {
    /// 编码宽度
    #[arg(long, default_value_t = 1920)]
    w: u32,
    /// 编码高度
    #[arg(long, default_value_t = 1080)]
    h: u32,
    /// 要编码的帧数
    #[arg(long, default_value_t = 30)]
    frames: u32,
    /// 帧率
    #[arg(long, default_value_t = 30)]
    fps: u32,
    /// 目标码率（kbps）
    #[arg(long, default_value_t = 8000)]
    bitrate: u32,
    /// 关键帧间隔（帧）
    #[arg(long, default_value_t = 30)]
    keyframe: u32,
    /// 强制使用软件编码（默认优先硬件）
    #[arg(long)]
    software: bool,
    /// 编码完成后解码验证
    #[arg(long)]
    decode: bool,
    /// 逐帧打印编码输出（诊断用）
    #[arg(long)]
    verbose: bool,
    /// 输出文件路径
    #[arg(long, default_value = r"D:\agent working\dafeiyu\.tmp\codec.h264")]
    out: PathBuf,
}

/// 生成一帧 BGRA8 合成画面：横向彩条 + 纵向亮度梯度 + 移动亮条 + 细颗粒噪声
/// + 棋盘状硬边色块。
///
/// 噪声与硬边是为了接近真实屏幕内容的熵，否则纯渐变会被编码器压到几百字节一帧，
/// 无法反映真实码率与文件大小。
fn synth_bgra(width: u32, height: u32, index: u32) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let bar_center = ((index as usize * 23) % w.max(1)) as i32;
    let mut buf = vec![0u8; w * h * 4];
    for y in 0..h {
        let vy = (y * 200 / h.max(1)) as i32;
        for x in 0..w {
            let vx = (x * 55 / w.max(1)) as i32;
            // 移动亮条：距离中心 40 像素内提亮。
            let boost = {
                let d = (x as i32 - bar_center).abs();
                if d < 40 {
                    90 - d * 2
                } else {
                    0
                }
            };
            // 确定性噪声（xorshift，不引入额外依赖）。
            let mut seed = (x as u32).wrapping_mul(1973)
                ^ (y as u32).wrapping_mul(9277)
                ^ index.wrapping_mul(26699)
                ^ 0x9E37_79B9;
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let noise = (seed & 0x3F) as i32 - 32;
            // 硬边色块（模拟文字/图标边缘，显著提升码率）。
            let checker = if (x / 64 + y / 64) % 7 == 0 { 60 } else { 0 };
            let r = (vx + boost + noise / 2).clamp(0, 255) as u8;
            let g = (vy + boost / 2 + checker + noise).clamp(0, 255) as u8;
            let b = (255 - vy + noise / 3).clamp(0, 255) as u8;
            let o = (y * w + x) * 4;
            buf[o] = b;
            buf[o + 1] = g;
            buf[o + 2] = r;
            buf[o + 3] = 255;
        }
    }
    buf
}

/// 扫描 Annex-B 码流的 NAL 类型统计（演示脚本自带，不依赖 crate 内部模块）。
fn scan_nal_types(data: &[u8]) -> Vec<(u8, usize)> {
    let mut counts = Vec::new();
    let mut i = 0usize;
    while i + 3 < data.len() {
        let sc3 = data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1;
        let sc4 = i + 4 < data.len()
            && data[i] == 0
            && data[i + 1] == 0
            && data[i + 2] == 0
            && data[i + 3] == 1;
        if sc3 || sc4 {
            let nal_start = if sc4 { i + 4 } else { i + 3 };
            if nal_start < data.len() {
                let t = data[nal_start] & 0x1F;
                match counts.iter_mut().find(|(k, _)| *k == t) {
                    Some((_, n)) => *n += 1,
                    None => counts.push((t, 1)),
                }
            }
            i = nal_start + 1;
            continue;
        }
        i += 1;
    }
    counts.sort_unstable();
    counts
}

/// NV12 亮度均值（用于验证解码结果不是黑屏）。
fn mean_luma(frame: &DecodedFrame) -> f64 {
    let y_len = frame
        .data
        .len()
        .min(frame.stride as usize * frame.height as usize);
    if y_len == 0 {
        return 0.0;
    }
    let sum: u64 = frame.data[..y_len].iter().map(|&b| b as u64).sum();
    sum as f64 / y_len as f64
}

fn main() -> Result<()> {
    let args = Args::parse();

    println!("== dfy-codec 编码演示 ==");
    println!(
        "尺寸：{}x{}，帧数 {}，fps {}，码率 {} kbps",
        args.w, args.h, args.frames, args.fps, args.bitrate
    );

    let list = available_encoders();
    if list.is_empty() {
        println!("探测到的 H.264 编码 MFT：无（本机可能没有 Media Foundation 编码器）");
    } else {
        println!("探测到的 H.264 编码 MFT：");
        for e in &list {
            println!(
                "  - {} [{}] {}",
                e.name,
                if e.hardware { "硬件" } else { "软件" },
                if e.available { "可用" } else { "不可用" }
            );
        }
    }

    let cfg = EncoderConfig {
        codec: Codec::H264,
        width: args.w,
        height: args.h,
        fps: args.fps,
        bitrate_kbps: args.bitrate,
        keyframe_interval: args.keyframe,
        prefer_hardware: !args.software,
    };

    let mut enc = create_encoder(&cfg).context("创建编码器失败")?;
    let frame_dur_us = 1_000_000u64 / args.fps as u64;
    let stride = args.w * 4;

    let mut stream: Vec<u8> = Vec::new();
    let mut encoded_count = 0usize;
    let mut keyframes = 0usize;
    let mut extra_seen = false;
    let mut latencies_ms: Vec<f64> = Vec::new();
    let start = Instant::now();

    for i in 0..args.frames {
        let bgra = synth_bgra(args.w, args.h, i);
        let t0 = Instant::now();
        let frames = enc
            .encode(&bgra, stride, i as u64 * frame_dur_us)
            .with_context(|| format!("编码第 {i} 帧失败"))?;
        latencies_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
        if args.verbose {
            let bytes: usize = frames.iter().map(|f| f.data.len()).sum();
            let keys = frames.iter().filter(|f| f.is_keyframe).count();
            let pts: Vec<u64> = frames.iter().map(|f| f.pts_us).collect();
            println!(
                "  第 {i} 帧 → {} 个输出，{bytes} 字节，关键帧 {keys}，pts {pts:?}，耗时 {:.2} ms",
                frames.len(),
                t0.elapsed().as_secs_f64() * 1000.0
            );
        }
        for f in frames {
            encoded_count += 1;
            if f.is_keyframe {
                keyframes += 1;
            }
            if f.extra.sps.is_some() && f.extra.pps.is_some() {
                extra_seen = true;
            }
            stream.extend_from_slice(&f.data);
        }
    }

    // 部分编码 MFT（Windows 自带 H.264 软编）内部有十几帧前瞻缓冲，
    // 必须 flush 才能拿到尾部帧。
    let flushed = enc.flush().context("冲刷编码器失败")?;
    let flushed_count = flushed.len();
    for f in flushed {
        encoded_count += 1;
        if f.is_keyframe {
            keyframes += 1;
        }
        if f.extra.sps.is_some() && f.extra.pps.is_some() {
            extra_seen = true;
        }
        stream.extend_from_slice(&f.data);
    }
    let elapsed = start.elapsed();
    if flushed_count > 0 {
        println!("冲刷出缓冲帧：{flushed_count} 帧（编码 MFT 内部前瞻缓冲）");
    }

    if let Some(parent) = args.out.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&args.out, &stream)
        .with_context(|| format!("写文件失败：{}", args.out.display()))?;

    let total = stream.len();
    let avg = total.checked_div(encoded_count).unwrap_or(0);
    println!("-- 编码结果 --");
    println!(
        "输出码流：{} 字节（{:.1} KB）",
        total,
        total as f64 / 1024.0
    );
    println!("输出帧数：{encoded_count}（关键帧 {keyframes} 帧）");
    println!("平均帧大小：{avg} 字节");
    println!(
        "SPS/PPS 是否随帧携带：{}",
        if extra_seen { "是" } else { "否" }
    );
    println!(
        "总耗时：{:.1} ms（{:.2} fps）",
        elapsed.as_secs_f64() * 1000.0,
        encoded_count as f64 / elapsed.as_secs_f64()
    );
    if !latencies_ms.is_empty() {
        let max = latencies_ms.iter().cloned().fold(f64::MIN, f64::max);
        let sum: f64 = latencies_ms.iter().sum();
        println!(
            "单帧编码耗时：平均 {:.2} ms，最大 {max:.2} ms",
            sum / latencies_ms.len() as f64
        );
    }
    println!("NAL 类型统计（type: 次数）：{:?}", scan_nal_types(&stream));
    let has_idr = scan_nal_types(&stream).iter().any(|(t, _)| *t == 5);
    println!(
        "文件路径：{}（>100KB 视为编码成功：{}）",
        args.out.display(),
        total > 100 * 1024
    );
    println!("含 IDR(type 5)：{has_idr}");

    if args.decode {
        println!("-- 解码验证 --");
        let mut dec = create_decoder(Codec::H264, !args.software).context("创建解码器失败")?;
        let t0 = Instant::now();
        let mut decoded = 0usize;
        let mut first: Option<DecodedFrame> = None;
        // 整条 Annex-B 码流一次喂入即可：解码器会连续吐出所有已解码帧。
        let frames = dec.decode(&stream).context("解码失败")?;
        for f in frames {
            decoded += 1;
            if first.is_none() {
                first = Some(f);
            }
        }
        println!(
            "解出帧数：{decoded}，耗时 {:.1} ms",
            t0.elapsed().as_secs_f64() * 1000.0
        );
        if let Some(f) = first {
            println!(
                "首帧：{}x{} stride={} format={} pts={}us 亮度均值 {:.1}",
                f.width,
                f.height,
                f.stride,
                f.format,
                f.pts_us,
                mean_luma(&f)
            );
        }
    }

    if total == 0 {
        anyhow::bail!("dfy-codec: 编码器没有输出任何数据");
    }
    Ok(())
}
