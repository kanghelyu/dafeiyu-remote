// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! macOS VideoToolbox H.264 真实编解码集成测试。
#![cfg(target_os = "macos")]

use std::time::Instant;

use dfy_codec::{
    available_encoders, create_decoder, create_encoder, Codec, DecodedFrame, EncoderConfig,
    PixelFormatOut,
};

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
            buf[o] = (255 - vy).clamp(0, 255) as u8;
            buf[o + 1] = (vy / 2 + 40).clamp(0, 255) as u8;
            buf[o + 2] = (vx + boost).clamp(0, 255) as u8;
            buf[o + 3] = 255;
        }
    }
    (buf, (w * 4) as u32)
}

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

fn annexb_to_avcc(annexb: &[u8]) -> Vec<u8> {
    let mut nals = Vec::new();
    let mut i = 0usize;
    let mut starts = Vec::new();
    while i + 2 < annexb.len() {
        if annexb[i] == 0 && annexb[i + 1] == 0 && annexb[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else if i + 3 < annexb.len()
            && annexb[i] == 0
            && annexb[i + 1] == 0
            && annexb[i + 2] == 0
            && annexb[i + 3] == 1
        {
            starts.push(i + 4);
            i += 4;
        } else {
            i += 1;
        }
    }
    for (idx, &start) in starts.iter().enumerate() {
        let mut end = starts
            .get(idx + 1)
            .map(|&next| {
                if next >= 4 && annexb[next - 4..next] == [0, 0, 0, 1] {
                    next - 4
                } else {
                    next - 3
                }
            })
            .unwrap_or(annexb.len());
        while end > start && annexb[end - 1] == 0 {
            end -= 1;
        }
        if end > start {
            nals.push(&annexb[start..end]);
        }
    }
    let mut avcc = Vec::new();
    for n in nals {
        avcc.extend_from_slice(&(n.len() as u32).to_be_bytes());
        avcc.extend_from_slice(n);
    }
    avcc
}

#[test]
fn available_encoders_reports_videotoolbox() {
    let list = available_encoders();
    println!("探测到 {} 个 macOS VideoToolbox H.264 编码器：", list.len());
    for e in &list {
        println!(
            "  - {} [{}] available={}",
            e.name,
            if e.hardware { "硬件" } else { "软件" },
            e.available
        );
        assert_eq!(e.codec, Codec::H264);
        assert!(!e.name.is_empty());
    }
    assert!(
        !list.is_empty(),
        "macOS 应至少有一个 VideoToolbox H.264 编码器"
    );
}

#[test]
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
    let mut enc = create_encoder(&cfg).expect("创建 VideoToolbox H.264 编码器");
    let frame_dur_us = 1_000_000u64 / FPS as u64;
    let mut stream = Vec::new();
    let mut encoded = 0usize;
    let mut keyframes = 0usize;
    let mut first_extra = None;
    let mut pts_list = Vec::new();
    let start = Instant::now();

    for i in 0..FRAMES {
        let (bgra, stride) = synth_bgra(W, H, i);
        let out = enc
            .encode(&bgra, stride, u64::from(i) * frame_dur_us)
            .expect("编码失败");
        for f in out {
            encoded += 1;
            if f.is_keyframe {
                keyframes += 1;
            }
            if f.extra.sps.is_some() && f.extra.pps.is_some() {
                first_extra = Some(f.extra.clone());
            }
            assert!(!f.data.is_empty());
            pts_list.push(f.pts_us);
            stream.extend_from_slice(&f.data);
        }
    }
    for f in enc.flush().expect("flush 编码器") {
        encoded += 1;
        if f.is_keyframe {
            keyframes += 1;
        }
        pts_list.push(f.pts_us);
        stream.extend_from_slice(&f.data);
    }

    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    println!(
        "VideoToolbox 编码：{encoded} 帧 / {} 字节，关键帧 {keyframes}，耗时 {elapsed_ms:.1} ms",
        stream.len()
    );
    assert_eq!(encoded, FRAMES as usize);
    assert_eq!(pts_list.first().copied(), Some(0));
    assert!(pts_list.windows(2).all(|w| w[0] < w[1]));
    assert!(keyframes >= 1);
    assert!(stream.len() > 10 * 1024);

    let extra = first_extra.expect("关键帧应携带 SPS/PPS");
    assert!(extra.sps.as_ref().is_some_and(|s| !s.is_empty()));
    assert!(extra.pps.as_ref().is_some_and(|p| !p.is_empty()));
    assert!(extra.codec_private.as_ref().is_some_and(|c| !c.is_empty()));

    let counts = nal_type_counts(&stream);
    assert!(counts.iter().any(|(t, _)| *t == 7), "应含 SPS(7)");
    assert!(counts.iter().any(|(t, _)| *t == 8), "应含 PPS(8)");
    assert!(counts.iter().any(|(t, _)| *t == 5), "应含 IDR(5)");

    let mut dec = create_decoder(Codec::H264, true).expect("创建 VideoToolbox 解码器");
    let mut decoded = dec.decode(&stream).expect("解码整条码流");
    decoded.extend(dec.flush().expect("flush 解码器"));
    assert_eq!(decoded.len(), FRAMES as usize);

    let first = &decoded[0];
    assert_eq!(first.width, W);
    assert_eq!(first.height, H);
    assert_eq!(first.format, PixelFormatOut::Nv12);
    assert_eq!(first.stride, W);
    assert_eq!(first.data.len(), (W * H + W * H / 2) as usize);
    let luma = mean_luma(first);
    assert!((20.0..=230.0).contains(&luma), "首帧亮度均值异常：{luma}");
}

#[test]
fn decoder_low_latency_single_frame_without_flush() {
    const W: u32 = 320;
    const H: u32 = 240;
    let cfg = EncoderConfig {
        codec: Codec::H264,
        width: W,
        height: H,
        fps: 30,
        bitrate_kbps: 1000,
        keyframe_interval: 60,
        prefer_hardware: true,
    };
    let mut enc = create_encoder(&cfg).unwrap();
    let mut dec = create_decoder(Codec::H264, true).unwrap();

    for i in 0..4u32 {
        let (bgra, stride) = synth_bgra(W, H, i);
        let encoded = enc.encode(&bgra, stride, u64::from(i) * 33_333).unwrap();
        assert_eq!(encoded.len(), 1);
        let decoded = dec.decode(&encoded[0].data).unwrap();
        assert_eq!(decoded.len(), 1, "单帧应立即解码输出，无需 flush");
        assert_eq!(decoded[0].width, W);
        assert_eq!(decoded[0].height, H);
    }
}

#[test]
fn request_keyframe_and_dynamic_bitrate() {
    const W: u32 = 640;
    const H: u32 = 360;
    let cfg = EncoderConfig {
        codec: Codec::H264,
        width: W,
        height: H,
        fps: 30,
        bitrate_kbps: 1500,
        keyframe_interval: 300,
        prefer_hardware: true,
    };
    let mut enc = create_encoder(&cfg).unwrap();
    let (bgra0, stride) = synth_bgra(W, H, 0);
    let f0 = enc.encode(&bgra0, stride, 0).unwrap();
    assert!(f0.iter().any(|f| f.is_keyframe));

    let (bgra1, _) = synth_bgra(W, H, 1);
    let f1 = enc.encode(&bgra1, stride, 33_333).unwrap();
    assert!(f1.iter().all(|f| !f.is_keyframe));

    enc.request_keyframe();
    let (bgra2, _) = synth_bgra(W, H, 2);
    let f2 = enc.encode(&bgra2, stride, 66_666).unwrap();
    assert!(
        f2.iter().any(|f| f.is_keyframe),
        "request_keyframe 应产生 IDR"
    );

    enc.set_bitrate(3000).expect("动态码率应生效");
    assert_eq!(enc.config().unwrap().bitrate_kbps, 3000);
    assert!(enc.set_bitrate(0).is_err());
    enc.set_frame_rate(60).expect("动态帧率应生效");
    assert_eq!(enc.config().unwrap().fps, 60);
    assert!(enc.set_frame_rate(121).is_err());
}

#[test]
fn decoder_accepts_avcc_input() {
    const W: u32 = 320;
    const H: u32 = 240;
    let cfg = EncoderConfig {
        codec: Codec::H264,
        width: W,
        height: H,
        fps: 30,
        bitrate_kbps: 1000,
        keyframe_interval: 30,
        prefer_hardware: true,
    };
    let mut enc = create_encoder(&cfg).unwrap();
    let (bgra, stride) = synth_bgra(W, H, 5);
    let encoded = enc.encode(&bgra, stride, 0).unwrap();
    let avcc = annexb_to_avcc(&encoded[0].data);
    let mut dec = create_decoder(Codec::H264, true).unwrap();
    let decoded = dec.decode(&avcc).expect("AVCC 码流应正常解码");
    assert_eq!(decoded.len(), 1);
    assert_eq!(decoded[0].width, W);
    assert_eq!(decoded[0].height, H);
}
