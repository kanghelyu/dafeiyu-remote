// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 像素格式转换（CPU，BT.709 limited range）。
//!
//! 采集端（DXGI）拿到的是 BGRA8；Media Foundation 的 H.264 编码 MFT 只吃
//! NV12。本模块提供手写的 BGRA/RGBA → NV12 转换：一次遍历同时算亮度与色度，
//! 每 2 行合并一次色度（NV12 的 4:2:0 要求），不分配临时整帧缓冲。
//!
//! 颜色约定：BT.709、limited range（Y: 16..235，UV: 16..240）。
//! 定点系数（8.8 精度）：
//!
//! * `Y'  = 0.2126 R + 0.7152 G + 0.0722 B`（全范围 0..255）
//! * `Y   = 16 + 219/255 * Y'`
//! * `Cb  = 128 + 224/255 * (B - Y') / 1.8556`
//! * `Cr  = 128 + 224/255 * (R - Y') / 1.5748`

use anyhow::{bail, Result};

/// NV12 每像素平均字节数（Y 1 字节 + UV 每 4 像素 2 字节）。
pub(crate) fn nv12_len(width: u32, height: u32) -> Option<usize> {
    let pixels = (width as usize).checked_mul(height as usize)?;
    pixels.checked_add(pixels / 2)
}

/// 校验转换参数并返回 `(row_bytes, dst_len)`。
fn check_params(
    src: &[u8],
    stride: u32,
    width: u32,
    height: u32,
    dst_len: usize,
) -> Result<(usize, usize)> {
    if width == 0 || height == 0 {
        bail!("dfy-codec: 图像尺寸不能为 0（{width}x{height}）");
    }
    if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        bail!("dfy-codec: NV12 要求宽高为偶数（收到 {width}x{height}）");
    }
    let row_bytes = width as usize * 4;
    let stride = stride as usize;
    if stride < row_bytes {
        bail!("dfy-codec: stride {stride} 小于一行字节数 {row_bytes}");
    }
    // 最后一行只要求包含有效像素，不要求含尾部 padding。
    let needed = (height as usize - 1)
        .checked_mul(stride)
        .and_then(|v| v.checked_add(row_bytes));
    let needed = needed.unwrap_or(usize::MAX);
    if src.len() < needed {
        bail!(
            "dfy-codec: 输入缓冲太小：需要 {needed} 字节，实得 {}",
            src.len()
        );
    }
    let need_dst = nv12_len(width, height).unwrap_or(usize::MAX);
    if dst_len < need_dst {
        bail!("dfy-codec: 输出缓冲太小：需要 {need_dst} 字节，实得 {dst_len}");
    }
    Ok((row_bytes, need_dst))
}

/// 载入一个像素的 RGB 分量（按 `RGBA` 决定字节序）。
#[inline(always)]
fn load_rgb(p: &[u8], rgba: bool) -> (i32, i32, i32) {
    // SAFETY: 调用方已保证切片至少有 4 字节。
    let (r, g, b) = if rgba {
        (p[0], p[1], p[2])
    } else {
        (p[2], p[1], p[0])
    };
    (r as i32, g as i32, b as i32)
}

/// BT.709 亮度（全范围 0..255）。
#[inline(always)]
fn luma_full(r: i32, g: i32, b: i32) -> i32 {
    (54 * r + 183 * g + 18 * b + 128) >> 8
}

/// BT.709 limited-range 亮度（16..235）。
#[inline(always)]
fn luma_limited(yf: i32) -> i32 {
    // (yf * 219 / 255)，用 257/65536 近似 1/255。
    16 + (((yf * 219 + 128) * 257) >> 16)
}

#[inline(always)]
fn clamp_u8(v: i32) -> u8 {
    if v < 0 {
        0
    } else if v > 255 {
        255
    } else {
        v as u8
    }
}

/// 4:2:0 色度：4 个像素的 `b - y'`（或 `r - y'`）均值 → limited range UV。
#[inline(always)]
fn chroma_limited(sum: i32, coeff: i32) -> u8 {
    // sum 是 4 个像素的差值和；先除 4（四舍五入），再乘系数缩放到 224/255。
    let avg = (sum + 2) >> 2;
    clamp_u8(128 + (((avg * coeff) + 128) >> 8))
}

/// 通用实现：`RGBA = true` 时源为 RGBA8，否则 BGRA8。
fn convert<const RGBA: bool>(
    src: &[u8],
    stride: u32,
    width: u32,
    height: u32,
    dst: &mut [u8],
) -> Result<()> {
    let (row_bytes, dst_len) = check_params(src, stride, width, height, dst.len())?;
    let w = width as usize;
    let h = height as usize;
    let stride = stride as usize;
    let dst = &mut dst[..dst_len];
    // Y 平面与 UV 平面互不重叠，一次切分后各行再按需切分。
    let (yplane_all, uvplane_all) = dst.split_at_mut(w * h);

    for y in (0..h).step_by(2) {
        let row0 = &src[y * stride..y * stride + row_bytes];
        let row1 = &src[(y + 1) * stride..(y + 1) * stride + row_bytes];
        // 相邻两行 Y 连续，一次 split_at_mut 拿到互不重叠的两个可变切片。
        let (top_part, bot_part) = yplane_all.split_at_mut((y + 1) * w);
        let ytop = &mut top_part[y * w..(y + 1) * w];
        let ybot = &mut bot_part[..w];
        let uv_start = (y / 2) * w;
        let uvrow = &mut uvplane_all[uv_start..uv_start + w];

        for x in (0..w).step_by(2) {
            let (r0, g0, b0) = load_rgb(&row0[x * 4..x * 4 + 4], RGBA);
            let (r1, g1, b1) = load_rgb(&row0[x * 4 + 4..x * 4 + 8], RGBA);
            let (r2, g2, b2) = load_rgb(&row1[x * 4..x * 4 + 4], RGBA);
            let (r3, g3, b3) = load_rgb(&row1[x * 4 + 4..x * 4 + 8], RGBA);

            let f0 = luma_full(r0, g0, b0);
            let f1 = luma_full(r1, g1, b1);
            let f2 = luma_full(r2, g2, b2);
            let f3 = luma_full(r3, g3, b3);

            ytop[x] = clamp_u8(luma_limited(f0));
            ytop[x + 1] = clamp_u8(luma_limited(f1));
            ybot[x] = clamp_u8(luma_limited(f2));
            ybot[x + 1] = clamp_u8(luma_limited(f3));

            // Cb 用 B - Y'，Cr 用 R - Y'。
            let cb_sum = (b0 - f0) + (b1 - f1) + (b2 - f2) + (b3 - f3);
            let cr_sum = (r0 - f0) + (r1 - f1) + (r2 - f2) + (r3 - f3);
            uvrow[x] = chroma_limited(cb_sum, 121);
            uvrow[x + 1] = chroma_limited(cr_sum, 143);
        }
    }
    Ok(())
}

/// 将 BGRA8（DXGI 桌面采集的默认格式）转换为 NV12。
///
/// * `src`：按 `stride` 字节/行排列的 BGRA8 像素，至少 `(height-1)*stride + width*4`
///   字节（最后一行的尾部 padding 可省略）。
/// * 返回紧凑排列的 NV12：先 `width*height` 字节 Y 平面，再 `width*height/2`
///   字节交织 UV 平面（U 在前）。
///
/// 宽高必须为偶数；颜色为 BT.709 limited range。
pub fn bgra_to_nv12(src: &[u8], stride: u32, width: u32, height: u32) -> Result<Vec<u8>> {
    let mut dst = vec![0u8; nv12_len(width, height).unwrap_or(0)];
    bgra_to_nv12_into(&mut dst, src, stride, width, height)?;
    Ok(dst)
}

/// [`bgra_to_nv12`] 的免分配版本：写入调用方提供的 `dst`。
pub fn bgra_to_nv12_into(
    dst: &mut [u8],
    src: &[u8],
    stride: u32,
    width: u32,
    height: u32,
) -> Result<()> {
    convert::<false>(src, stride, width, height, dst)
}

/// 将 RGBA8 转换为 NV12（BT.709 limited range）。
pub fn rgba_to_nv12(src: &[u8], stride: u32, width: u32, height: u32) -> Result<Vec<u8>> {
    let mut dst = vec![0u8; nv12_len(width, height).unwrap_or(0)];
    rgba_to_nv12_into(&mut dst, src, stride, width, height)?;
    Ok(dst)
}

/// [`rgba_to_nv12`] 的免分配版本：写入调用方提供的 `dst`。
pub fn rgba_to_nv12_into(
    dst: &mut [u8],
    src: &[u8],
    stride: u32,
    width: u32,
    height: u32,
) -> Result<()> {
    convert::<true>(src, stride, width, height, dst)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_bgra(width: u32, height: u32, b: u8, g: u8, r: u8, a: u8) -> Vec<u8> {
        let mut v = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..width * height {
            v.extend_from_slice(&[b, g, r, a]);
        }
        v
    }

    fn y_at(nv12: &[u8], width: u32, x: u32, y: u32) -> u8 {
        nv12[(y * width + x) as usize]
    }

    fn uv_at(nv12: &[u8], width: u32, height: u32, x: u32, y: u32) -> (u8, u8) {
        let off = (width * height + (y / 2) * width + (x / 2) * 2) as usize;
        (nv12[off], nv12[off + 1])
    }

    #[test]
    fn gray_uses_limited_range() {
        let src = solid_bgra(16, 8, 128, 128, 128, 255);
        let nv12 = bgra_to_nv12(&src, 64, 16, 8).unwrap();
        assert_eq!(nv12.len(), 16 * 8 + 16 * 8 / 2);
        // 中灰 128 → Y ≈ 16 + 219*128/255 ≈ 126，UV ≈ 128。
        for y in 0..8 {
            for x in 0..16 {
                let yy = y_at(&nv12, 16, x, y);
                assert!((yy as i32 - 126).abs() <= 1, "Y={yy} @({x},{y})");
            }
        }
        for y in 0..8 {
            for x in 0..16 {
                let (u, v) = uv_at(&nv12, 16, 8, x, y);
                assert!((u as i32 - 128).abs() <= 1, "U={u}");
                assert!((v as i32 - 128).abs() <= 1, "V={v}");
            }
        }
    }

    #[test]
    fn black_and_white_hit_limited_endpoints() {
        let black = bgra_to_nv12(&solid_bgra(4, 4, 0, 0, 0, 255), 16, 4, 4).unwrap();
        let white = bgra_to_nv12(&solid_bgra(4, 4, 255, 255, 255, 255), 16, 4, 4).unwrap();
        assert!((y_at(&black, 4, 0, 0) as i32 - 16).abs() <= 1);
        assert!((y_at(&white, 4, 0, 0) as i32 - 235).abs() <= 1);
        let (u, v) = uv_at(&black, 4, 4, 0, 0);
        assert!((u as i32 - 128).abs() <= 1 && (v as i32 - 128).abs() <= 1);
    }

    #[test]
    fn pure_red_bt709() {
        let nv12 = bgra_to_nv12(&solid_bgra(16, 8, 0, 0, 255, 255), 64, 16, 8).unwrap();
        // Y = 16 + 0.8588 * 0.2126*255 ≈ 62.6；Cb ≈ 102.3；Cr ≈ 240.0
        assert!((y_at(&nv12, 16, 0, 0) as i32 - 63).abs() <= 2);
        let (u, v) = uv_at(&nv12, 16, 8, 0, 0);
        assert!((u as i32 - 102).abs() <= 2, "U={u}");
        assert!((v as i32 - 240).abs() <= 2, "V={v}");
    }

    #[test]
    fn rgba_order_is_honoured() {
        // RGBA 源：R=255,G=0,B=0 → 与 BGRA 红色等价。
        let mut src = Vec::new();
        for _ in 0..64 {
            src.extend_from_slice(&[255, 0, 0, 255]);
        }
        let a = rgba_to_nv12(&src, 32, 8, 8).unwrap();
        let b = bgra_to_nv12(&solid_bgra(8, 8, 0, 0, 255, 255), 32, 8, 8).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn stride_with_padding_is_handled() {
        // 4 像素宽（16 字节/行），stride 32 字节，填充垃圾。
        let mut src = Vec::new();
        for _ in 0..4 {
            for x in 0..4 {
                src.extend_from_slice(&[0, 0, 0, 255]);
                if x == 0 {
                    src.extend_from_slice(&[0xEE; 16]);
                }
            }
        }
        // 保证每行 32 字节，共 4 行、8 行？height=4。
        let nv12 = bgra_to_nv12(&src, 32, 4, 4).unwrap();
        assert_eq!(nv12.len(), 4 * 4 + 4 * 4 / 2);
        assert!(!nv12.contains(&0xEE));
    }

    #[test]
    fn rejects_bad_geometry() {
        let src = vec![0u8; 64];
        assert!(bgra_to_nv12(&src, 16, 3, 4).is_err()); // 奇数宽
        assert!(bgra_to_nv12(&src, 8, 4, 4).is_err()); // stride 太小
        assert!(bgra_to_nv12(&src, 16, 8, 8).is_err()); // 源太小
        let mut dst = vec![0u8; 8];
        assert!(bgra_to_nv12_into(&mut dst, &src, 16, 4, 4).is_err()); // 目标太小
    }

    #[test]
    fn perf_2800x1840() {
        // 性能冒烟：debug 下放宽（CI 抖动），release 下要求 < 30ms。
        let (w, h) = (2800u32, 1840u32);
        let stride = w * 4;
        let mut src = vec![0u8; (stride * h) as usize];
        for (i, b) in src.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let mut dst = vec![0u8; nv12_len(w, h).unwrap()];
        // 预热
        bgra_to_nv12_into(&mut dst, &src, stride, w, h).unwrap();
        let start = std::time::Instant::now();
        bgra_to_nv12_into(&mut dst, &src, stride, w, h).unwrap();
        let elapsed = start.elapsed();
        let budget_ms = if cfg!(debug_assertions) { 400 } else { 30 };
        assert!(
            elapsed.as_millis() < budget_ms,
            "BGRA→NV12 {w}x{h} 用了 {elapsed:?}（预算 {budget_ms}ms）"
        );
    }
}
