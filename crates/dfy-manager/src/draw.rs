// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (C) 2026 Dafeiyu Remote contributors

//! 无窗口依赖的绘制原语：软件像素混合、文本布局、列表行几何。
//!
//! 渲染目标与 `dfy-ui` 一致：softbuffer 的 `u32` 像素为 `0x00RRGGBB`。

use ab_glyph::{Font, FontVec, PxScale, ScaleFont};

/// 软件帧缓冲视图：把宽高与像素切片绑在一起，避免大量位置参数。
pub struct Surface<'a> {
    pub buffer: &'a mut [u32],
    pub width: u32,
    pub height: u32,
}

impl<'a> Surface<'a> {
    pub fn new(buffer: &'a mut [u32], width: u32, height: u32) -> Self {
        Self {
            buffer,
            width,
            height,
        }
    }

    pub fn fill(&mut self, color: u32) {
        self.buffer.fill(color);
    }

    /// 填充矩形（越界部分裁掉）。
    pub fn fill_rect(&mut self, x: u32, y: u32, w: u32, h: u32, color: u32) {
        for row in y..y.saturating_add(h).min(self.height) {
            let start = row as usize * self.width as usize + x.min(self.width) as usize;
            let end = (row as usize * self.width as usize
                + x.saturating_add(w).min(self.width) as usize)
                .min(self.buffer.len());
            if start < end {
                self.buffer[start..end].fill(color);
            }
        }
    }

    /// 绘制一行文本。`x` 为左上角，`y` 为字体基线以上的起始行。
    pub fn draw_text(&mut self, font: &FontVec, text: &str, x: f32, y: f32, size: f32, color: u32) {
        let scaled = font.as_scaled(PxScale::from(size));
        let baseline = y + scaled.ascent();
        let mut pen_x = x;
        for ch in text.chars() {
            let id = scaled.glyph_id(ch);
            let mut glyph = scaled.scaled_glyph(ch);
            glyph.position = ab_glyph::point(pen_x, baseline);
            if let Some(outlined) = font.outline_glyph(glyph) {
                let bounds = outlined.px_bounds();
                let origin_x = bounds.min.x as i32;
                let origin_y = bounds.min.y as i32;
                let (width, height) = (self.width, self.height);
                let buffer = &mut self.buffer;
                outlined.draw(|gx, gy, coverage| {
                    let px = origin_x + gx as i32;
                    let py = origin_y + gy as i32;
                    if px < 0 || py < 0 {
                        return;
                    }
                    let (px, py) = (px as u32, py as u32);
                    if px >= width || py >= height {
                        return;
                    }
                    let index = py as usize * width as usize + px as usize;
                    if let Some(pixel) = buffer.get_mut(index) {
                        *pixel = blend(color, *pixel, (coverage.clamp(0.0, 1.0) * 255.0) as u8);
                    }
                });
            }
            pen_x += scaled.h_advance(id);
        }
    }
}

/// 把覆盖度 `coverage` 的前景色混合到背景像素上。
#[must_use]
pub fn blend(foreground: u32, background: u32, coverage: u8) -> u32 {
    let a = u32::from(coverage.clamp(0, 255));
    let inv = 255 - a;
    let r = (((foreground >> 16) & 0xFF) * a + ((background >> 16) & 0xFF) * inv) / 255;
    let g = (((foreground >> 8) & 0xFF) * a + ((background >> 8) & 0xFF) * inv) / 255;
    let b = ((foreground & 0xFF) * a + (background & 0xFF) * inv) / 255;
    (r << 16) | (g << 8) | b
}

/// 第 `index` 行的顶部 Y 坐标。
#[must_use]
pub const fn row_y(header_height: u32, row_height: u32, index: usize) -> u32 {
    header_height + row_height * index as u32
}

/// 根据点击的 Y 坐标算列表行号；页眉/超出范围返回 `None`。
#[must_use]
pub const fn row_from_y(
    y: u32,
    header_height: u32,
    row_height: u32,
    count: usize,
) -> Option<usize> {
    if y < header_height {
        return None;
    }
    let index = (y - header_height) / row_height;
    if (index as usize) < count {
        Some(index as usize)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blend_endpoints_and_midpoint() {
        assert_eq!(blend(0x00FF0000, 0x00000000, 255), 0x00FF0000);
        assert_eq!(blend(0x00FF0000, 0x00000000, 0), 0);
        assert_eq!(blend(0x00FFFFFF, 0x00000000, 128), 0x00808080);
    }

    #[test]
    fn row_geometry_maps_clicks_to_rows() {
        assert_eq!(row_y(100, 24, 2), 148);
        assert_eq!(row_from_y(99, 100, 24, 3), None);
        assert_eq!(row_from_y(100, 100, 24, 3), Some(0));
        assert_eq!(row_from_y(124, 100, 24, 3), Some(1));
        assert_eq!(row_from_y(172, 100, 24, 3), None);
    }

    #[test]
    fn fill_rect_clips_to_surface() {
        let mut pixels = vec![0u32; 4 * 4];
        let mut surface = Surface::new(&mut pixels, 4, 4);
        surface.fill_rect(3, 3, 2, 2, 0x00FF0000);
        assert_eq!(surface.buffer[3 * 4 + 3], 0x00FF0000);
        assert_eq!(surface.buffer[0], 0);
    }
}
