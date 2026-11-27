// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// 大肥鱼远程 —— DIB（CF_DIB / CF_DIBV5）到 PNG 的转换。
// 跨端图像统一走 PNG：Windows 侧读 CF_DIB 时在这里即时转换，避免对端实现 DIB 解析。
// 支持范围：8/16/24/32bpp，BI_RGB 与 BI_BITFIELDS；不支持 1/4bpp、RLE、BITMAPCOREHEADER。

use anyhow::{anyhow, bail, Context, Result};

/// 只支持 `BITMAPINFOHEADER`（40 字节）及更长的 DIB 头。
const BITMAPINFOHEADER_LEN: u32 = 40;

const BI_RGB: u32 = 0;
const BI_BITFIELDS: u32 = 3;

/// 把一段 DIB 内存（`CF_DIB` 的原始字节，不含 BITMAPFILEHEADER）转换成 PNG 字节流。
///
/// 输出统一为 8bit RGB（32bpp 的 alpha 通道被忽略：`CF_DIB` 里 alpha 语义不可靠，
/// 多数截图工具填 0，直接透传会得到全透明图）。
pub fn dib_to_png(dib: &[u8]) -> Result<Vec<u8>> {
    let info = parse_header(dib)?;
    let rgb = decode_pixels(dib, &info)?;
    encode_png(&rgb, info.width, info.height)
        .with_context(|| format!("PNG 编码失败（{}x{}）", info.width, info.height))
}

struct DibInfo {
    width: u32,
    height: u32,
    /// `biHeight > 0` 表示自下而上存储（Windows 默认），否则自上而下。
    bottom_up: bool,
    bpp: u16,
    mask_rgb: Option<[u32; 3]>,
    palette: Vec<[u8; 3]>,
    pixel_offset: usize,
    stride: usize,
}

fn parse_header(dib: &[u8]) -> Result<DibInfo> {
    if dib.len() < BITMAPINFOHEADER_LEN as usize {
        bail!("DIB 太短（{} 字节），连 BITMAPINFOHEADER 都不够", dib.len());
    }
    let header_size = read_u32(dib, 0)?;
    if header_size < BITMAPINFOHEADER_LEN {
        bail!("不支持的 DIB 头长度 {header_size}（需 >= 40，BITMAPCOREHEADER 不支持）");
    }
    if dib.len() < header_size as usize {
        bail!(
            "DIB 头声明 {header_size} 字节，但实际只有 {} 字节",
            dib.len()
        );
    }

    let width_i32 = read_i32(dib, 4)?;
    if width_i32 <= 0 {
        bail!("DIB 宽度非法：{width_i32}");
    }
    let width = width_i32 as u32;

    let height_i32 = read_i32(dib, 8)?;
    if height_i32 == 0 {
        bail!("DIB 高度非法：0");
    }
    let bottom_up = height_i32 > 0;
    let height = height_i32.unsigned_abs();

    let bpp = read_u16(dib, 14)?;
    if !matches!(bpp, 8 | 16 | 24 | 32) {
        bail!("不支持的 DIB 位深：{bpp}bpp（支持 8/16/24/32）");
    }

    let compression = read_u32(dib, 16)?;

    // 调色板（仅 8bpp 需要）：紧跟头部，每项 BGRA 4 字节。
    let mut palette = Vec::new();
    let mut extra_after_header = 0usize;
    if bpp == 8 {
        let clr_used = read_u32(dib, 32)?;
        let count = if clr_used == 0 {
            256
        } else {
            clr_used as usize
        };
        let start = header_size as usize;
        let bytes = count * 4;
        if start + bytes > dib.len() {
            bail!(
                "DIB 调色板越界（需要 {bytes} 字节，实际 {}",
                dib.len() - start
            );
        }
        palette.reserve(count);
        for i in 0..count {
            let at = start + i * 4;
            palette.push([dib[at + 2], dib[at + 1], dib[at]]);
        }
        extra_after_header += bytes;
    }

    let mask_rgb = match compression {
        BI_RGB => None,
        BI_BITFIELDS => {
            if !matches!(bpp, 16 | 32) {
                bail!("BI_BITFIELDS 只支持 16/32bpp（当前 {bpp}bpp）");
            }
            // BITMAPV4/V5 头的通道掩码在头部内部（偏移 40），否则紧随 40 字节头。
            let mask_at = if header_size >= 52 {
                40usize
            } else {
                header_size as usize
            };
            if mask_at + 12 > dib.len() {
                bail!("DIB BI_BITFIELDS 掩码越界");
            }
            let masks = [
                read_u32(dib, mask_at)?,
                read_u32(dib, mask_at + 4)?,
                read_u32(dib, mask_at + 8)?,
            ];
            if header_size < 52 {
                extra_after_header += 12;
            }
            if masks.iter().all(|m| *m == 0) {
                bail!("DIB BI_BITFIELDS 掩码全为 0");
            }
            Some(masks)
        }
        other => bail!("不支持的 DIB 压缩方式 {other}（仅支持 BI_RGB/BI_BITFIELDS）"),
    };

    let pixel_offset = header_size as usize + extra_after_header;
    let stride = (width as usize * bpp as usize).div_ceil(32) * 4;
    let needed = pixel_offset + stride * height as usize;
    if needed > dib.len() {
        bail!(
            "DIB 像素数据不足：需要 {needed} 字节（含头部/调色板），实际 {} 字节",
            dib.len()
        );
    }

    Ok(DibInfo {
        width,
        height,
        bottom_up,
        bpp,
        mask_rgb,
        palette,
        pixel_offset,
        stride,
    })
}

fn decode_pixels(dib: &[u8], info: &DibInfo) -> Result<Vec<u8>> {
    let width = info.width as usize;
    let height = info.height as usize;
    let mut out = Vec::with_capacity(width * height * 3);
    let bpp = info.bpp as usize;
    let pixel_bytes = bpp / 8;

    for y in 0..height {
        let src_row = if info.bottom_up { height - 1 - y } else { y };
        let row_start = info.pixel_offset + src_row * info.stride;
        let row = dib
            .get(row_start..row_start + width * pixel_bytes)
            .ok_or_else(|| anyhow!("DIB 第 {src_row} 行越界"))?;
        for x in 0..width {
            let px = &row[x * pixel_bytes..(x + 1) * pixel_bytes];
            let rgb = match bpp {
                8 => {
                    let idx = px[0] as usize;
                    info.palette.get(idx).copied().unwrap_or([0, 0, 0])
                }
                16 => {
                    let v = u16::from_le_bytes([px[0], px[1]]) as u32;
                    match info.mask_rgb {
                        Some(masks) => [
                            apply_mask(v, masks[0]),
                            apply_mask(v, masks[1]),
                            apply_mask(v, masks[2]),
                        ],
                        // 16bpp BI_RGB 约定为 RGB555。
                        None => [
                            (((v >> 10) & 0x1f) * 255 / 31) as u8,
                            (((v >> 5) & 0x1f) * 255 / 31) as u8,
                            ((v & 0x1f) * 255 / 31) as u8,
                        ],
                    }
                }
                24 => [px[2], px[1], px[0]],
                32 => {
                    let v = u32::from_le_bytes([px[0], px[1], px[2], px[3]]);
                    match info.mask_rgb {
                        Some(masks) => [
                            apply_mask(v, masks[0]),
                            apply_mask(v, masks[1]),
                            apply_mask(v, masks[2]),
                        ],
                        // 32bpp BI_RGB 的 BGRX：忽略最高字节（通常是未定义的 alpha）。
                        None => [px[2], px[1], px[0]],
                    }
                }
                _ => unreachable!("parse_header 已限定位深"),
            };
            out.extend_from_slice(&rgb);
        }
    }
    Ok(out)
}

/// 按掩码取出通道并线性缩放到 0..=255。
fn apply_mask(value: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    let max = mask >> shift;
    if max == 0 {
        return 0;
    }
    let raw = (value & mask) >> shift;
    ((raw as u64 * 255) / max as u64) as u8
}

fn encode_png(rgb: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(rgb)?;
    }
    Ok(out)
}

fn read_u16(bytes: &[u8], at: usize) -> Result<u16> {
    let b = bytes
        .get(at..at + 2)
        .ok_or_else(|| anyhow!("DIB 读取越界（u16 @ {at}）"))?;
    Ok(u16::from_le_bytes([b[0], b[1]]))
}

fn read_u32(bytes: &[u8], at: usize) -> Result<u32> {
    let b = bytes
        .get(at..at + 4)
        .ok_or_else(|| anyhow!("DIB 读取越界（u32 @ {at}）"))?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn read_i32(bytes: &[u8], at: usize) -> Result<i32> {
    Ok(read_u32(bytes, at)? as i32)
}
