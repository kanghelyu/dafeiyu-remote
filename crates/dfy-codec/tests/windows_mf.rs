// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 真实 Media Foundation 集成测试（需要本机有 H.264 MFT）。
//!
//! 默认跑 `available_encoders()` 探测；真编码/解码用 `#[ignore]` 标记：
//!
//! ```text
//! cargo test -p dfy-codec -- --ignored --nocapture
//! ```
#![cfg(windows)]

use std::time::Instant;

use dfy_codec::{
    available_encoders, bgra_to_nv12, create_decoder, create_encoder, Codec, DecodedFrame,
    EncoderConfig,
};

/// 合成一帧 BGRA8 渐变画面（横向渐变 + 移动亮条），返回 (像素, stride)。
fn synth_bgra(width: u32, height: u32, index: u32) -> (Vec<u8>, u32) {
    let w = width as usize;
    let h = height as usize;
    let bar = (index as usize * 37) % w.max(1);
    let mut buf = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let vx = (x * 255 / w.max(1)) as i32;
            let vy = (y * 255 / h.max(1)) as i32;
            let d = (x as i32 - bar as i32).abs();
            let boost = if d < 60 { 120 - d * 2 } else { 0 };
            let o = (y * w + x) * 4;
            buf[o] = (255 - vy).clamp(0, 255) as u8; // B
            buf[o + 1] = (vy / 2 + 40).clamp(0, 255) as u8; // G
            buf[o + 2] = (vx + boost).clamp(0, 255) as u8; // R
            buf[o + 3] = 255;
        }
    }
    (buf, (w * 4) as u32)
}

/// 扫描 Annex-B 的 NAL 类型计数。
fn nal_type_counts(data: &[u8]) -> Vec<(u8, usize)> {
    let mut counts: Vec<(u8, usize)> = Vec::new();
    let mut i = 0usize;
    while i + 3 < data.len() {
        let sc4 = i + 4 < data.len()
            && data[i] == 0
            && data[i + 1] == 0
            && data[i + 2] == 0
            && data[i + 3] == 1;
        let sc3 = data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1;
        if sc3 || sc4 {
            let start = if sc4 { i + 4 } else { i + 3 };
            if start < data.len() {
                let t = data[start] & 0x1F;
                match counts.iter_mut().find(|(k, _)| *k == t) {
                    Some((_, n)) => *n += 1,
                    None => counts.push((t, 1)),
                }
            }
            i = start + 1;
            continue;
        }
        i += 1;
    }
    counts.sort_unstable();
    counts
}

fn mean_luma(f: &DecodedFrame) -> f64 {
    let len = f.data.len().min(f.stride as usize * f.height as usize);
    if len == 0 {
        return 0.0;
    }
    let sum: u64 = f.data[..len].iter().map(|&b| b as u64).sum();
    sum as f64 / len as f64
}

/// 非忽略测试：枚举本机编码器，不要求非空（CI 机器可能没有 MF 编解码器）。
#[test]
fn available_encoders_does_not_panic() {
    let list = available_encoders();
    println!("探测到 {} 个 H.264 编码 MFT：", list.len());
    for e in &list {
        println!(
            "  - {} [{}] {}",
            e.name,
            if e.hardware { "硬件" } else { "软件" },
            if e.available { "可用" } else { "不可用" }
        );
        assert_eq!(e.codec, Codec::H264);
        assert!(!e.name.is_empty(), "MFT 名称不应为空");
    }
    if list.is_empty() {
        eprintln!("警告：本机没有 H.264 编码 MFT（Media Foundation 编解码器缺失）");
    }
}

/// 真编码 + 真解码：1280×720 30 帧 → 统计码流 → 解回 NV12 校验尺寸/亮度。
#[test]
#[ignore = "需要本机安装 Media Foundation H.264 编码/解码器"]
fn real_encode_then_decode_1280x720() {
    const W: u32 = 1280;
    const H: u32 = 720;
    const FRAMES: u32 = 30;
    const FPS: u32 = 30;

    let cfg = EncoderConfig {
        codec: Codec::H264,
        width: W,
        height: H,
        fps: FPS,
        bitrate_kbps: 4000,
        keyframe_interval: 15,
        prefer_hardware: true,
    };
    let mut enc = match create_encoder(&cfg) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("本机没有可用的 H.264 编码 MFT，跳过真编码测试：{e:#}");
            return;
        }
    };

    let frame_dur_us = 1_000_000u64 / FPS as u64;
    let mut stream: Vec<u8> = Vec::new();
    let mut encoded = 0usize;
    let mut keyframes = 0usize;
    let mut first_extra = None;
    let mut max_latency_ms = 0.0f64;
    let start = Instant::now();

    let mut pts_list: Vec<u64> = Vec::new();
    for i in 0..FRAMES {
        let (bgra, stride) = synth_bgra(W, H, i);
        // 顺便验证 CPU 转换路径：BGRA → NV12 再直接喂编码器。
        let t0 = Instant::now();
        let out = enc
            .encode(&bgra, stride, i as u64 * frame_dur_us)
            .unwrap_or_else(|e| panic!("编码第 {i} 帧失败：{e:#}"));
        max_latency_ms = max_latency_ms.max(t0.elapsed().as_secs_f64() * 1000.0);
        for f in out {
            encoded += 1;
            if f.is_keyframe {
                keyframes += 1;
            }
            if f.extra.sps.is_some() && f.extra.pps.is_some() {
                first_extra = Some(f.extra.clone());
            }
            assert!(!f.data.is_empty(), "编码帧不能为空");
            // 编码 MFT 有前瞻缓冲，输出帧的 pts 对应更早喂入的帧，但必须单调递增且不超前。
            assert!(
                f.pts_us <= i as u64 * frame_dur_us,
                "输出 pts 超前于当前输入：{} > {}",
                f.pts_us,
                i as u64 * frame_dur_us
            );
            pts_list.push(f.pts_us);
            stream.extend_from_slice(&f.data);
        }
    }
    let flush_out = enc.flush().expect("冲刷编码器失败");
    if !flush_out.is_empty() {
        println!("冲刷出缓冲帧：{} 帧", flush_out.len());
    }
    for f in flush_out {
        encoded += 1;
        if f.is_keyframe {
            keyframes += 1;
        }
        if f.extra.sps.is_some() && f.extra.pps.is_some() {
            first_extra = Some(f.extra.clone());
        }
        pts_list.push(f.pts_us);
        stream.extend_from_slice(&f.data);
    }

    let encode_ms = start.elapsed().as_secs_f64() * 1000.0;

    println!(
        "编码：{encoded} 帧 / {} 字节，关键帧 {keyframes}，总耗时 {encode_ms:.1} ms，最大单帧 {max_latency_ms:.2} ms",
        stream.len()
    );
    println!("NAL 统计：{:?}", nal_type_counts(&stream));

    assert!(
        encoded >= FRAMES as usize - 2,
        "编码器丢帧过多：{encoded}/{FRAMES}"
    );
    assert_eq!(pts_list.first().copied(), Some(0), "首帧 pts 应为 0");
    assert!(
        pts_list.windows(2).all(|w| w[0] < w[1]),
        "pts 应严格递增：{pts_list:?}"
    );
    assert!(keyframes >= 1, "30 帧里没有任何关键帧");
    assert!(
        stream.len() > 20 * 1024,
        "码流过小（{} 字节），疑似编码失败",
        stream.len()
    );

    // SPS/PPS 必须在关键帧之后可得。
    let extra = first_extra.expect("关键帧应携带 SPS/PPS");
    assert!(extra.sps.as_ref().map(|s| !s.is_empty()).unwrap_or(false));
    assert!(extra.pps.as_ref().map(|s| !s.is_empty()).unwrap_or(false));

    let counts = nal_type_counts(&stream);
    assert!(counts.iter().any(|(t, _)| *t == 7), "码流里应有 SPS(7)");
    assert!(counts.iter().any(|(t, _)| *t == 8), "码流里应有 PPS(8)");
    assert!(counts.iter().any(|(t, _)| *t == 5), "码流里应有 IDR(5)");

    // ---- 解码 ----
    let mut dec = match create_decoder(Codec::H264, true) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("本机没有可用的 H.264 解码 MFT，跳过解码验证：{e:#}");
            return;
        }
    };
    let start = Instant::now();
    let mut decoded: Vec<DecodedFrame> = Vec::new();
    // 整条 Annex-B 流一次喂入：解码器会连续吐出所有已解码帧（与 examples 一致）。
    match dec.decode(&stream) {
        Ok(frames) => decoded.extend(frames),
        Err(e) => panic!("解码失败：{e:#}"),
    }
    // 微软 H.264 解码 MFT 会攒十几帧才吐输出，必须冲刷才会全部出来。
    match dec.flush() {
        Ok(frames) => {
            println!("解码冲刷出 {} 帧", frames.len());
            decoded.extend(frames);
        }
        Err(e) => panic!("冲刷解码器失败：{e:#}"),
    }
    let decode_ms = start.elapsed().as_secs_f64() * 1000.0;

    println!("解码：{} 帧，耗时 {decode_ms:.1} ms", decoded.len());
    assert!(!decoded.is_empty(), "解码器没有输出任何帧");
    let first = &decoded[0];
    println!(
        "首帧：{}x{} stride={} format={} pts={} 亮度均值 {:.1}",
        first.width,
        first.height,
        first.stride,
        first.format,
        first.pts_us,
        mean_luma(first)
    );
    assert_eq!(first.width, W, "解码宽度应等于编码宽度");
    assert_eq!(first.height, H, "解码高度应等于编码高度");
    assert_eq!(first.format, dfy_codec::PixelFormatOut::Nv12);
    assert!(
        first.data.len() >= first.stride as usize * first.height as usize,
        "NV12 数据不足以覆盖 Y 平面"
    );
    // 合成的渐变画面亮度均值在 16..235 之间且非全黑。
    let luma = mean_luma(first);
    assert!((20.0..=230.0).contains(&luma), "首帧亮度均值异常：{luma}");
    assert!(
        decoded.len() >= FRAMES as usize - 5,
        "解出帧数过少：{} / {FRAMES}",
        decoded.len()
    );
}

#[test]
#[ignore = "需要本机安装 Media Foundation H.264 编码/解码器"]
fn decoder_low_latency_does_not_require_drain() {
    const W: u32 = 320;
    const H: u32 = 240;
    const FRAMES: u32 = 24;

    let cfg = EncoderConfig {
        codec: Codec::H264,
        width: W,
        height: H,
        fps: 30,
        bitrate_kbps: 1000,
        keyframe_interval: 300,
        prefer_hardware: false,
    };
    let mut encoder = match create_encoder(&cfg) {
        Ok(encoder) => encoder,
        Err(error) => {
            eprintln!("本机没有可用的 H.264 编码 MFT，跳过：{error:#}");
            return;
        }
    };
    let mut encoded = Vec::new();
    for index in 0..FRAMES {
        let (bgra, stride) = synth_bgra(W, H, index);
        encoded.extend(
            encoder
                .encode(&bgra, stride, index as u64 * 33_333)
                .expect("编码失败"),
        );
    }
    encoded.extend(encoder.flush().expect("冲刷编码器失败"));
    assert!(encoded.len() >= 4, "编码输出不足：{}", encoded.len());

    let mut decoder = match create_decoder(Codec::H264, false) {
        Ok(decoder) => decoder,
        Err(error) => {
            eprintln!("本机没有可用的 H.264 解码 MFT，跳过：{error:#}");
            return;
        }
    };
    let mut decoded = Vec::new();
    for frame in encoded.iter().take(4) {
        decoded.extend(decoder.decode(&frame.data).expect("解码失败"));
    }
    assert!(
        !decoded.is_empty(),
        "低延迟解码器不应要求 COMMAND_DRAIN 才输出首帧"
    );
    assert_eq!(decoded[0].width, W);
    assert_eq!(decoded[0].height, H);
}

/// 真编码：验证 `request_keyframe` 与 `set_bitrate` 的行为。
#[test]
#[ignore = "需要本机安装 Media Foundation H.264 编码器"]
fn request_keyframe_and_bitrate() {
    const W: u32 = 640;
    const H: u32 = 360;
    let cfg = EncoderConfig {
        codec: Codec::H264,
        width: W,
        height: H,
        fps: 30,
        bitrate_kbps: 1500,
        keyframe_interval: 300, // 故意放很长，靠 request_keyframe 触发
        prefer_hardware: true,
    };
    let mut enc = match create_encoder(&cfg) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("本机没有可用的 H.264 编码 MFT，跳过：{e:#}");
            return;
        }
    };
    let stride = W * 4;
    let mut out = Vec::new();
    // 微软软编 MFT 约有十几帧前瞻缓冲；不能要求前 5 帧就吐出输出。
    for i in 0..20u32 {
        let (bgra, _) = synth_bgra(W, H, i);
        out.extend(
            enc.encode(&bgra, stride, i as u64 * 33_333)
                .expect("编码失败"),
        );
    }
    if let Ok(flushed) = enc.flush() {
        out.extend(flushed);
    }
    assert!(!out.is_empty(), "前 20 帧及冲刷后仍没有任何输出");

    enc.request_keyframe();
    let (bgra, _) = synth_bgra(W, H, 20);
    let mut out = enc.encode(&bgra, stride, 20 * 33_333).expect("编码失败");
    // 编码 MFT 有前瞻缓冲，请求的关键帧可能还在缓冲里，冲刷出来才能看到。
    match enc.flush() {
        Ok(mut flushed) => out.append(&mut flushed),
        Err(e) => eprintln!("冲刷编码器失败（不影响关键帧判断）：{e:#}"),
    }
    let force_ok = out.iter().any(|f| f.is_keyframe);
    println!("request_keyframe 后是否出现关键帧：{force_ok}");

    // set_bitrate：部分 MFT 不支持 Codec API，失败是可接受的，但必须给出明确原因。
    match enc.set_bitrate(3000) {
        Ok(()) => {
            assert_eq!(enc.config().unwrap().bitrate_kbps, 3000);
            println!("set_bitrate(3000) 成功");
        }
        Err(e) => {
            let message = e.to_string();
            assert!(
                message.contains("Codec API")
                    || message.contains("CODECAPI_AVEncCommonMeanBitRate")
                    || message.contains("设置失败"),
                "码率控制错误不够明确：{message}"
            );
            println!("set_bitrate(3000) 不受支持：{e:#}");
        }
    }
    assert!(force_ok, "request_keyframe 之后冲刷出的帧里应有关键帧");
}

/// 真编码器：120fps 配置可创建，但当前 MFT 上的动态帧率修改必须明确要求重建。
#[test]
#[ignore = "需要本机安装 Media Foundation H.264 编码器"]
fn native_120fps_config_and_live_frame_rate_error() {
    let cfg = EncoderConfig {
        codec: Codec::H264,
        width: 640,
        height: 360,
        fps: 120,
        bitrate_kbps: 3000,
        keyframe_interval: 120,
        prefer_hardware: true,
    };
    let mut enc = match create_encoder(&cfg) {
        Ok(enc) => enc,
        Err(err) => {
            eprintln!("本机没有可用的 H.264 编码 MFT，跳过：{err:#}");
            return;
        }
    };
    assert_eq!(enc.config().unwrap().fps, 120);
    enc.set_frame_rate(120).expect("相同帧率无需重建");
    let invalid = enc.set_frame_rate(121).unwrap_err().to_string();
    assert!(invalid.contains("1..=120"));
    let live_change = enc.set_frame_rate(60).unwrap_err().to_string();
    assert!(live_change.contains("Media Foundation") && live_change.contains("重建"));
    assert_eq!(enc.config().unwrap().fps, 120);
}

/// CPU 转换冒烟：BGRA→NV12 尺寸与亮度范围。
#[test]
fn bgra_to_nv12_smoke() {
    let w = 64u32;
    let h = 32u32;
    let (bgra, stride) = synth_bgra(w, h, 0);
    let nv12 = bgra_to_nv12(&bgra, stride, w, h).expect("转换失败");
    assert_eq!(nv12.len(), (w * h + w * h / 2) as usize);
    let y = &nv12[..(w * h) as usize];
    assert!(
        y.iter().all(|&v| (16..=235).contains(&v)),
        "Y 应落在 limited range"
    );
}
