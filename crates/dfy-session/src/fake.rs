// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 测试/自检替身：不依赖真实屏幕与编解码器，用来验证会话编排（采集 → 发送 → 重组 → 解码）。

use std::time::{Duration, Instant};

use crate::capture_loop::RuntimeSettings;
use crate::{Capturer, DecodedFrame, EncodedFrame, Encoder, RawFrame};

/// 合成帧的魔数（码流里必须能找到，便于测试断言）。
pub const FAKE_MAGIC: &[u8] = b"DFYF";

/// 假采集器：按固定间隔产出合成 BGRA 帧。
pub struct FakeCapturer {
    width: u32,
    height: u32,
    remaining: u64,
    counter: u64,
    interval: Duration,
    last: Instant,
}

impl FakeCapturer {
    /// 无限帧。
    pub fn new(width: u32, height: u32, frames: u64) -> Self {
        Self {
            width,
            height,
            remaining: frames,
            counter: 0,
            interval: Duration::from_millis(10),
            last: Instant::now() - Duration::from_millis(10),
        }
    }

    /// 已产出帧数。
    pub fn produced(&self) -> u64 {
        self.counter
    }
}

impl Capturer for FakeCapturer {
    fn dimensions(&self) -> Option<(u32, u32)> {
        Some((self.width, self.height))
    }

    fn capture(&mut self, timeout_ms: u32) -> anyhow::Result<Option<RawFrame>> {
        if self.remaining == 0 {
            // 模拟超时无新帧，同时让调用方有机会检查停止标志
            std::thread::sleep(Duration::from_millis(u64::from(timeout_ms).min(5)));
            return Ok(None);
        }
        let since = self.last.elapsed();
        if since < self.interval {
            std::thread::sleep(self.interval - since);
        }
        self.last = Instant::now();
        self.remaining = self.remaining.saturating_sub(1);
        self.counter += 1;

        let width = self.width.max(2);
        let height = self.height.max(2);
        let stride = width * 4;
        let seed = (self.counter & 0xff) as u8;
        let mut data = vec![0u8; stride as usize * height as usize];
        // 左上角写入可辨识的 2×2 色块，其余填渐变（保证非零）
        for y in 0..height as usize {
            for x in 0..width as usize {
                let o = y * stride as usize + x * 4;
                data[o] = seed.wrapping_add(x as u8);
                data[o + 1] = seed.wrapping_add(y as u8);
                data[o + 2] = (x ^ y) as u8;
                data[o + 3] = 0xff;
            }
        }
        Ok(Some(RawFrame {
            width,
            height,
            stride,
            data,
            pts_us: dfy_transport::now_us(),
            full_frame: true,
        }))
    }
}

/// 假编码器：输出 `DFYF + pts_us(8B) + 原始像素摘要`，每 10 帧一个关键帧。
pub struct FakeEncoder {
    frame_index: u64,
    keyframe_interval: u64,
    pending_keyframe: bool,
    settings: RuntimeSettings,
}

impl Default for FakeEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeEncoder {
    /// 关键帧间隔 10 帧。
    pub fn new() -> Self {
        Self {
            frame_index: 0,
            keyframe_interval: 10,
            pending_keyframe: false,
            settings: RuntimeSettings::new(60, 4000),
        }
    }

    /// 使用指定初始运行时设置。
    pub fn with_settings(settings: RuntimeSettings) -> Self {
        settings.validate().expect("invalid fake encoder settings");
        Self {
            settings,
            ..Self::new()
        }
    }

    /// 当前运行时设置。
    pub fn settings(&self) -> RuntimeSettings {
        self.settings
    }
}

impl Encoder for FakeEncoder {
    fn encode(&mut self, frame: &RawFrame) -> anyhow::Result<Vec<EncodedFrame>> {
        self.frame_index += 1;
        let keyframe = self.pending_keyframe || self.frame_index % self.keyframe_interval == 1;
        self.pending_keyframe = false;

        let mut data = Vec::with_capacity(FAKE_MAGIC.len() + 8 + 64);
        data.extend_from_slice(FAKE_MAGIC);
        data.extend_from_slice(&frame.pts_us.to_be_bytes());
        data.extend_from_slice(&frame.width.to_be_bytes());
        data.extend_from_slice(&frame.height.to_be_bytes());
        // 只取前 64 字节像素做摘要，码流保持小体积
        let n = frame.data.len().min(64);
        data.extend_from_slice(&frame.data[..n]);
        Ok(vec![EncodedFrame {
            data,
            keyframe,
            pts_us: frame.pts_us,
        }])
    }

    fn request_keyframe(&mut self) {
        self.pending_keyframe = true;
    }

    fn set_bitrate(&mut self, kbps: u32) -> anyhow::Result<()> {
        let mut settings = self.settings;
        settings.bitrate_kbps = kbps;
        settings.validate()?;
        self.settings = settings;
        Ok(())
    }

    fn set_frame_rate(&mut self, fps: u32) -> anyhow::Result<()> {
        let mut settings = self.settings;
        settings.fps = fps;
        settings.validate()?;
        self.settings = settings;
        Ok(())
    }

    fn runtime_settings(&self) -> Option<RuntimeSettings> {
        Some(self.settings)
    }

    fn flush(&mut self) -> anyhow::Result<Vec<EncodedFrame>> {
        Ok(Vec::new())
    }
}

/// 假解码器：把假码流还原成固定 2×2 的 BGRA 图（真实像素内容不重要，帧数/pts 才重要）。
pub struct FakeDecoder {
    decoded: u64,
}

impl Default for FakeDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeDecoder {
    /// 新建。
    pub fn new() -> Self {
        Self { decoded: 0 }
    }

    /// 已解码帧数。
    pub fn decoded(&self) -> u64 {
        self.decoded
    }
}

impl crate::Decoder for FakeDecoder {
    fn decode(&mut self, data: &[u8], pts_us: u64) -> anyhow::Result<Option<DecodedFrame>> {
        anyhow::ensure!(
            data.starts_with(FAKE_MAGIC),
            "假解码器收到非法码流（缺少 DFYF 魔数）"
        );
        self.decoded += 1;
        let width: u32 = 2;
        let height: u32 = 2;
        let stride = width * 4;
        let mut rgba = vec![0u8; (stride * height) as usize];
        for px in rgba.chunks_exact_mut(4) {
            px[3] = 0xff;
        }
        Ok(Some(DecodedFrame {
            width,
            height,
            stride,
            data: rgba,
            pts_us,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Decoder;

    #[test]
    fn fake_roundtrip_produces_expected_frames() {
        let mut capturer = FakeCapturer::new(16, 8, 3);
        let mut encoder = FakeEncoder::new();
        let mut decoder = FakeDecoder::new();
        let mut count = 0;
        while let Some(frame) = capturer.capture(1).unwrap() {
            let outs = encoder.encode(&frame).unwrap();
            assert_eq!(outs.len(), 1);
            let decoded = decoder.decode(&outs[0].data, frame.pts_us).unwrap();
            assert!(decoded.is_some());
            count += 1;
        }
        assert_eq!(count, 3);
        assert_eq!(decoder.decoded(), 3);
    }
}
