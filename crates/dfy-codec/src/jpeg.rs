// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 纯 Rust JPEG / MJPEG 软件编解码后端（跨平台兜底）。
//!
//! 用途：在没有 Media Foundation（非 Windows）或硬件编码不可用时，用逐帧
//! JPEG（MJPEG）保证「采集 → 编码 → 传输 → 解码」链路仍可端到端跑通。
//! 每帧都是关键帧，因此丢包只会损失单帧，不需要等待 IDR；代价是码率明显
//! 高于 H.264，只作为兜底/局域网/低帧率场景使用。
//!
//! 输入像素格式与 [`VideoEncoder::encode`] 一致：4 字节/像素按 BGRA8，否则
//! 按 NV12。解码输出统一转成 NV12（与 H.264 解码器保持一致），方便上层复用
//! 同一套 `nv12 → BGRA` 显示转换。

use anyhow::{Context, Result};
use jpeg_decoder::PixelFormat as JpegPixelFormat;

use crate::{
    format::rgba_to_nv12, CodecExtra, DecodedFrame, EncodedFrame, EncoderConfig, PixelFormatOut,
    VideoDecoder, VideoEncoder,
};

/// 默认 JPEG 质量（0–100）。MJPEG 兜底优先保证画面可读，码率交给上层限帧。
pub const DEFAULT_QUALITY: u8 = 75;

/// 解码输出时间戳估计用的帧间隔（MJPEG 解码器拿不到真实 PTS）。
const ASSUMED_FRAME_US: u64 = 33_333;

/// MJPEG 编码器。
#[derive(Debug)]
pub struct JpegEncoder {
    cfg: EncoderConfig,
    quality: u8,
    frames: u64,
}

impl JpegEncoder {
    /// 按编码配置创建设备无关的 JPEG 编码器。
    pub fn new(cfg: &EncoderConfig) -> Result<Self> {
        cfg.validate()?;
        if cfg.width > u32::from(u16::MAX) || cfg.height > u32::from(u16::MAX) {
            anyhow::bail!(
                "dfy-codec: JPEG 编码尺寸超出上限（{}x{}，最大 65535）",
                cfg.width,
                cfg.height
            );
        }
        Ok(Self {
            cfg: cfg.clone(),
            quality: DEFAULT_QUALITY,
            frames: 0,
        })
    }

    /// 覆盖默认 JPEG 质量（0–100，超出范围自动截断）。
    pub fn with_quality(mut self, quality: u8) -> Self {
        self.quality = quality.min(100);
        self
    }
}

impl VideoEncoder for JpegEncoder {
    fn encode(&mut self, raw: &[u8], stride: u32, pts_us: u64) -> Result<Vec<EncodedFrame>> {
        let rgb = to_rgb24(raw, stride, self.cfg.width, self.cfg.height)?;
        let mut data = Vec::new();
        {
            let encoder = jpeg_encoder::Encoder::new(&mut data, self.quality);
            encoder
                .encode(
                    &rgb,
                    self.cfg.width as u16,
                    self.cfg.height as u16,
                    jpeg_encoder::ColorType::Rgb,
                )
                .map_err(|err| anyhow::anyhow!("dfy-codec: JPEG 编码失败：{err}"))?;
        }
        self.frames += 1;
        Ok(vec![EncodedFrame {
            data,
            // MJPEG 每帧自包含，全部按关键帧处理。
            is_keyframe: true,
            pts_us,
            extra: CodecExtra::default(),
        }])
    }

    fn request_keyframe(&mut self) {
        // 每帧都是关键帧，无需处理。
    }

    fn set_bitrate(&mut self, kbps: u32) -> Result<()> {
        crate::validate_bitrate_kbps(kbps)?;
        anyhow::bail!(
            "dfy-codec: MJPEG 没有目标码率控制器，无法动态设置 {kbps} kbps；请调整分辨率或帧率"
        )
    }

    fn config(&self) -> Option<&EncoderConfig> {
        Some(&self.cfg)
    }
}

/// MJPEG 解码器。
#[derive(Debug, Default)]
pub struct JpegDecoder {
    frames: u64,
}

impl JpegDecoder {
    /// 创建解码器。
    pub fn new() -> Self {
        Self::default()
    }
}

impl VideoDecoder for JpegDecoder {
    fn decode(&mut self, data: &[u8]) -> Result<Vec<DecodedFrame>> {
        if data.is_empty() {
            return Ok(Vec::new());
        }
        let mut decoder = jpeg_decoder::Decoder::new(data);
        let pixels = decoder
            .decode()
            .map_err(|err| anyhow::anyhow!("dfy-codec: JPEG 解码失败：{err}"))?;
        let info = decoder
            .info()
            .context("dfy-codec: JPEG 解码后缺少图像信息")?;
        let width = u32::from(info.width);
        let height = u32::from(info.height);
        if width == 0 || height == 0 {
            anyhow::bail!("dfy-codec: JPEG 解出空图像（{width}x{height}）");
        }
        let rgba = to_rgba32(&pixels, info.pixel_format, width, height)?;
        let nv12 = rgba_to_nv12(&rgba, width * 4, width, height)?;
        self.frames += 1;
        Ok(vec![DecodedFrame {
            data: nv12,
            stride: width,
            width,
            height,
            format: PixelFormatOut::Nv12,
            pts_us: self.frames * ASSUMED_FRAME_US,
        }])
    }
}

/// 把 BGRA8（4 字节/像素）或 NV12 输入统一转成紧凑 RGB24。
fn to_rgb24(raw: &[u8], stride: u32, width: u32, height: u32) -> Result<Vec<u8>> {
    let w = width as usize;
    let h = height as usize;
    let row = stride as usize;
    let mut rgb = vec![0u8; w * h * 3];

    if row >= w * 4 && raw.len() >= row * h {
        // BGRA8
        for y in 0..h {
            let src = &raw[y * row..y * row + w * 4];
            let dst = &mut rgb[y * w * 3..(y + 1) * w * 3];
            for x in 0..w {
                let s = &src[x * 4..x * 4 + 4];
                let d = &mut dst[x * 3..x * 3 + 3];
                d[0] = s[2];
                d[1] = s[1];
                d[2] = s[0];
            }
        }
        return Ok(rgb);
    }

    // NV12：Y 平面 `stride` 字节/行，UV 平面紧随其后（交织）。
    let needed = row * h + row * h.div_ceil(2);
    if row >= w && raw.len() >= needed && w.is_multiple_of(2) && h.is_multiple_of(2) {
        let y_plane = &raw[..row * h];
        let uv_plane = &raw[row * h..];
        for y in 0..h {
            for x in 0..w {
                let yy = f32::from(y_plane[y * row + x]);
                let uv_row = y / 2;
                let uv_off = uv_row * row + (x / 2) * 2;
                let u = f32::from(uv_plane[uv_off]) - 128.0;
                let v = f32::from(uv_plane[uv_off + 1]) - 128.0;
                // BT.601 有限范围逆变换，与显示侧保持一致即可。
                let c = (yy - 16.0) * 1.164;
                let r = (c + 1.596 * v).round().clamp(0.0, 255.0) as u8;
                let g = (c - 0.392 * u - 0.813 * v).round().clamp(0.0, 255.0) as u8;
                let b = (c + 2.017 * u).round().clamp(0.0, 255.0) as u8;
                let off = (y * w + x) * 3;
                rgb[off] = r;
                rgb[off + 1] = g;
                rgb[off + 2] = b;
            }
        }
        return Ok(rgb);
    }

    anyhow::bail!(
        "dfy-codec: JPEG 输入像素数据不足以覆盖 {width}x{height}（stride={stride}, len={}）",
        raw.len()
    )
}

/// 把 JPEG 解出的像素统一转成 RGBA8（紧凑，`width*4` 步长）。
fn to_rgba32(pixels: &[u8], format: JpegPixelFormat, width: u32, height: u32) -> Result<Vec<u8>> {
    let px = (width * height) as usize;
    let mut rgba = vec![0u8; px * 4];
    match format {
        JpegPixelFormat::RGB24 => {
            anyhow::ensure!(
                pixels.len() >= px * 3,
                "dfy-codec: JPEG RGB24 数据长度不足（{}/{px}）",
                pixels.len()
            );
            for i in 0..px {
                rgba[i * 4] = pixels[i * 3];
                rgba[i * 4 + 1] = pixels[i * 3 + 1];
                rgba[i * 4 + 2] = pixels[i * 3 + 2];
                rgba[i * 4 + 3] = 255;
            }
        }
        JpegPixelFormat::L8 => {
            anyhow::ensure!(
                pixels.len() >= px,
                "dfy-codec: JPEG L8 数据长度不足（{}/{px}）",
                pixels.len()
            );
            for i in 0..px {
                let v = pixels[i];
                rgba[i * 4] = v;
                rgba[i * 4 + 1] = v;
                rgba[i * 4 + 2] = v;
                rgba[i * 4 + 3] = 255;
            }
        }
        JpegPixelFormat::CMYK32 | JpegPixelFormat::L16 => {
            anyhow::bail!("dfy-codec: 暂不支持该 JPEG 像素格式：{format:?}");
        }
    }
    Ok(rgba)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EncoderConfig;

    fn cfg(width: u32, height: u32) -> EncoderConfig {
        EncoderConfig {
            codec: crate::Codec::Mjpeg,
            width,
            height,
            fps: 30,
            bitrate_kbps: 4000,
            keyframe_interval: 30,
            prefer_hardware: false,
        }
    }

    fn bgra_gradient(width: u32, height: u32) -> Vec<u8> {
        let mut buf = vec![0u8; (width * height * 4) as usize];
        for y in 0..height {
            for x in 0..width {
                let off = ((y * width + x) * 4) as usize;
                buf[off] = (x % 256) as u8;
                buf[off + 1] = (y % 256) as u8;
                buf[off + 2] = ((x + y) % 256) as u8;
                buf[off + 3] = 255;
            }
        }
        buf
    }

    #[test]
    fn encodes_bgra_and_decodes_to_nv12() {
        let (w, h) = (32u32, 24u32);
        let mut encoder = JpegEncoder::new(&cfg(w, h)).unwrap();
        let raw = bgra_gradient(w, h);
        let frames = encoder.encode(&raw, w * 4, 1_000).unwrap();
        assert_eq!(frames.len(), 1);
        assert!(frames[0].is_keyframe);
        assert_eq!(frames[0].pts_us, 1_000);
        assert!(frames[0].data.len() > 100);

        let mut decoder = JpegDecoder::new();
        let decoded = decoder.decode(&frames[0].data).unwrap();
        assert_eq!(decoded.len(), 1);
        let frame = &decoded[0];
        assert_eq!(frame.width, w);
        assert_eq!(frame.height, h);
        assert_eq!(frame.stride, w);
        assert_eq!(frame.format, PixelFormatOut::Nv12);
        assert_eq!(frame.data.len(), (w * h * 3 / 2) as usize);
        // 梯度画面解出的 Y 平面不应全黑。
        assert!(frame.data[..(w * h) as usize].iter().any(|v| *v > 16));
    }

    #[test]
    fn roundtrip_through_nv12_input() {
        let (w, h) = (16u32, 16u32);
        let raw = bgra_gradient(w, h);
        let nv12 = crate::format::bgra_to_nv12(&raw, w * 4, w, h).unwrap();
        let mut encoder = JpegEncoder::new(&cfg(w, h)).unwrap();
        let frames = encoder.encode(&nv12, w, 0).unwrap();
        let mut decoder = JpegDecoder::new();
        let decoded = decoder.decode(&frames[0].data).unwrap();
        assert_eq!(decoded[0].data.len(), (w * h * 3 / 2) as usize);
    }

    #[test]
    fn rejects_short_input() {
        let mut encoder = JpegEncoder::new(&cfg(64, 48)).unwrap();
        let err = encoder.encode(&[0u8; 10], 64 * 4, 0).unwrap_err();
        assert!(err.to_string().contains("数据不足"));
    }

    #[test]
    fn rejects_garbage_payload() {
        let mut decoder = JpegDecoder::new();
        assert!(decoder.decode(&[1, 2, 3, 4]).is_err());
    }

    #[test]
    fn empty_payload_is_ignored() {
        let mut decoder = JpegDecoder::new();
        assert!(decoder.decode(&[]).unwrap().is_empty());
    }
}
