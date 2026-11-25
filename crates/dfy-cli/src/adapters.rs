// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 把 `dfy-codec`（Media Foundation H.264）适配成 `dfy-session` 的编码/解码抽象。
//!
//! `dfy-session` 只约定「RawFrame(BGRA8) ↔ 码流 ↔ DecodedFrame(BGRA8)」，
//! 而 MFT 解码输出是 NV12，所以这里做一次颜色空间转换（BT.709 有限范围）。

use anyhow::Result;

use dfy_session::{
    DecodedFrame, Decoder as SessionDecoder, EncodedFrame, Encoder as SessionEncoder, RawFrame,
};

/// MFT H.264 编码器适配器（输入 BGRA8，输出 Annex-B）。
pub struct MftEncoderAdapter {
    inner: Box<dyn dfy_codec::VideoEncoder>,
}

impl MftEncoderAdapter {
    /// 按给定参数创建编码器（优先硬件，失败自动回退软编）。
    pub fn new(cfg: &dfy_codec::EncoderConfig) -> Result<Self> {
        Ok(Self {
            inner: dfy_codec::create_encoder(cfg)?,
        })
    }
}

impl SessionEncoder for MftEncoderAdapter {
    fn encode(&mut self, frame: &RawFrame) -> Result<Vec<EncodedFrame>> {
        let frames = self.inner.encode(&frame.data, frame.stride, frame.pts_us)?;
        Ok(frames
            .into_iter()
            .map(|f| EncodedFrame {
                data: f.data,
                keyframe: f.is_keyframe,
                pts_us: f.pts_us,
            })
            .collect())
    }

    fn request_keyframe(&mut self) {
        self.inner.request_keyframe();
    }
}

/// MFT H.264 解码器适配器（输出 BGRA8）。
pub struct MftDecoderAdapter {
    inner: Box<dyn dfy_codec::VideoDecoder>,
}

impl MftDecoderAdapter {
    /// 按给定编码格式创建解码器。
    pub fn with_codec(codec: dfy_codec::Codec, prefer_hardware: bool) -> Result<Self> {
        Ok(Self {
            inner: dfy_codec::create_decoder(codec, prefer_hardware)?,
        })
    }
}

impl SessionDecoder for MftDecoderAdapter {
    fn decode(&mut self, data: &[u8], pts_us: u64) -> Result<Option<DecodedFrame>> {
        let frames = self.inner.decode(data)?;
        let Some(f) = frames.into_iter().last() else {
            return Ok(None);
        };
        let stride = f.width * 4;
        Ok(Some(DecodedFrame {
            width: f.width,
            height: f.height,
            stride,
            data: nv12_to_bgra(&f.data, f.stride, f.width, f.height),
            pts_us,
        }))
    }
}

/// NV12 → BGRA8（BT.709 有限范围；逐像素标量实现，MVP 够用）。
pub fn nv12_to_bgra(nv12: &[u8], y_stride: u32, width: u32, height: u32) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let ys = y_stride.max(width) as usize;
    let uv_stride = ys.max(w);
    let y_plane = w * h;
    let uv_plane = uv_stride * h.div_ceil(2).max(1);
    let mut out = vec![0u8; w * h * 4];
    if nv12.len() < y_plane + uv_plane {
        return out;
    }
    let u_base = w * h;
    for row in 0..h {
        let y_row = row * ys;
        let uv_row = (row / 2) * uv_stride;
        let out_row = row * w * 4;
        for col in 0..w {
            let y = nv12[y_row + col] as f32;
            let uv_off = u_base + uv_row + (col / 2) * 2;
            let u = nv12[uv_off] as f32;
            let v = nv12[uv_off + 1] as f32;
            let c = (y - 16.0) * 1.164;
            let d = u - 128.0;
            let e = v - 128.0;
            let r = (c + 1.793 * e).clamp(0.0, 255.0) as u8;
            let g = (c - 0.213 * d - 0.533 * e).clamp(0.0, 255.0) as u8;
            let b = (c + 2.112 * d).clamp(0.0, 255.0) as u8;
            let o = out_row + col * 4;
            out[o] = b;
            out[o + 1] = g;
            out[o + 2] = r;
            out[o + 3] = 255;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nv12_black_is_dark() {
        // 16/128/128 是 BT.601/709 有限范围的黑色。
        let w = 4u32;
        let h = 2u32;
        let mut nv12 = vec![16u8; (w * h) as usize];
        nv12.extend(std::iter::repeat_n(128u8, ((w / 2) * h) as usize * 2));
        let bgra = nv12_to_bgra(&nv12, w, w, h);
        assert_eq!(bgra.len(), (w * h * 4) as usize);
        assert!(bgra
            .chunks(4)
            .all(|p| p[3] == 255 && p[0] < 8 && p[1] < 8 && p[2] < 8));
    }

    #[test]
    fn nv12_short_input_is_safe() {
        let bgra = nv12_to_bgra(&[0u8; 4], 4, 4, 4);
        assert_eq!(bgra.len(), 64);
    }
}
