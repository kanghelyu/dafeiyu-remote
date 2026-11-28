// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use crate::frame::Rect;
use ab_glyph::{Font, FontRef, Point, PxScale, Rect as Bounds, ScaleFont};

const FONT_DATA: &[u8] = include_bytes!("../../../assets/dfy-ui-sans.ttf");

struct RasterGlyph {
    bounds: Bounds,
    alpha: Vec<u8>,
}

fn font() -> &'static FontRef<'static> {
    static FONT: OnceLock<FontRef<'static>> = OnceLock::new();
    FONT.get_or_init(|| FontRef::try_from_slice(FONT_DATA).expect("bundled UI font"))
}

fn glyphs() -> &'static Mutex<HashMap<(char, u32), RasterGlyph>> {
    static GLYPHS: OnceLock<Mutex<HashMap<(char, u32), RasterGlyph>>> = OnceLock::new();
    GLYPHS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn pixel_scale(pixel_size: u32) -> PxScale {
    PxScale::from(pixel_size.max(1) as f32)
}

fn rasterize(character: char, pixel_size: u32) -> RasterGlyph {
    let font = font();
    let scale = pixel_scale(pixel_size);
    let scaled = font.as_scaled(scale);
    let glyph = font.glyph_id(character).with_scale_and_position(
        scale,
        Point {
            x: 0.0,
            y: scaled.ascent(),
        },
    );
    if let Some(glyph) = font.outline_glyph(glyph) {
        let bounds = glyph.px_bounds();
        let width = ((bounds.max.x - bounds.min.x) as f64).ceil().max(0.0) as u32;
        let height = ((bounds.max.y - bounds.min.y) as f64).ceil().max(0.0) as u32;
        let mut alpha = vec![0; (width as usize) * (height as usize)];
        glyph.draw(|x, y, coverage| {
            if x < width && y < height {
                alpha[(y as usize) * width as usize + x as usize] =
                    (coverage.clamp(0.0, 1.0) * 255.0).round() as u8;
            }
        });
        RasterGlyph { bounds, alpha }
    } else {
        RasterGlyph {
            bounds: Bounds::default(),
            alpha: Vec::new(),
        }
    }
}

pub(crate) fn text_width(text: &str, pixel_size: u32) -> f64 {
    let font = font();
    let scaled = font.as_scaled(pixel_scale(pixel_size));
    text.chars()
        .map(|character| scaled.h_advance(font.glyph_id(character)) as f64)
        .sum()
}

pub(crate) fn truncate(text: &str, pixel_size: u32, max_width: f64) -> String {
    if text_width(text, pixel_size) <= max_width || max_width <= 0.0 {
        return text.to_string();
    }
    let mut result = String::new();
    for character in text.chars() {
        let next = format!("{result}{character}");
        if text_width(&next, pixel_size) + text_width("…", pixel_size) > max_width {
            break;
        }
        result = next;
    }
    format!("{result}…")
}

pub(crate) fn draw(
    buffer: &mut [u32],
    buffer_width: u32,
    buffer_height: u32,
    rect: Rect,
    text: &str,
    pixel_size: u32,
    color: u32,
) {
    let pixel_size = pixel_size.max(1);
    let mut x = rect.x as f64;
    let baseline = rect.y as f64 + pixel_size as f64 * 0.88;
    for character in text.chars() {
        let mut cache = glyphs().lock().unwrap_or_else(|error| error.into_inner());
        let cached = cache
            .entry((character, pixel_size))
            .or_insert_with(|| rasterize(character, pixel_size));
        let glyph_width = (cached.bounds.max.x - cached.bounds.min.x).max(0.0) as usize;
        let glyph_height = (cached.bounds.max.y - cached.bounds.min.y).max(0.0) as usize;
        for row in 0..glyph_height {
            for column in 0..glyph_width {
                let alpha = cached
                    .alpha
                    .get(row * glyph_width + column)
                    .copied()
                    .unwrap_or(0);
                if alpha < 16 {
                    continue;
                }
                let px = (x + cached.bounds.min.x as f64 + column as f64).floor() as i32;
                let py = (baseline + cached.bounds.min.y as f64 + row as f64).floor() as i32;
                blend(buffer, buffer_width, buffer_height, px, py, color, alpha);
            }
        }
        x += text_width(&character.to_string(), pixel_size);
    }
}

fn blend(buffer: &mut [u32], width: u32, height: u32, x: i32, y: i32, color: u32, alpha: u8) {
    if x < 0 || y < 0 || x as u32 >= width || y as u32 >= height {
        return;
    }
    let index = y as usize * width as usize + x as usize;
    let Some(slot) = buffer.get_mut(index) else {
        return;
    };
    let amount = u32::from(alpha);
    let background = *slot;
    let blend = |shift: u32| {
        let fg = (color >> shift) & 255;
        let bg = (background >> shift) & 255;
        (fg * amount + bg * (255 - amount)) / 255
    };
    *slot = (blend(16) << 16) | (blend(8) << 8) | blend(0);
}

pub(crate) fn panel(
    buffer: &mut [u32],
    width: u32,
    height: u32,
    rect: Rect,
    fill: u32,
    border: u32,
    radius: u32,
) {
    if rect.width < 2 || rect.height < 2 {
        return;
    }
    let radius = radius.min(rect.width / 2).min(rect.height / 2);
    for y in 0..rect.height {
        let dy = if y < radius {
            radius - y - 1
        } else if y >= rect.height - radius {
            y.saturating_sub(rect.height - radius)
        } else {
            0
        };
        let dx = (radius as f64)
            .mul_add(radius as f64, -(dy as f64 * dy as f64))
            .sqrt();
        let inset = radius - dx.ceil() as u32;
        let left = rect.x + inset;
        let right = rect.x + rect.width - inset;
        for x in left..right {
            blend(
                buffer,
                width,
                height,
                x as i32,
                (rect.y + y) as i32,
                fill,
                255,
            );
        }
        if y > 0 && y + 1 < rect.height {
            blend(
                buffer,
                width,
                height,
                left as i32,
                (rect.y + y) as i32,
                border,
                255,
            );
            blend(
                buffer,
                width,
                height,
                (right - 1) as i32,
                (rect.y + y) as i32,
                border,
                255,
            );
        }
    }
    for x in rect.x + radius..rect.x + rect.width - radius {
        blend(buffer, width, height, x as i32, rect.y as i32, border, 255);
        blend(
            buffer,
            width,
            height,
            x as i32,
            (rect.y + rect.height - 1) as i32,
            border,
            255,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_font_renders_chinese_and_ascii() {
        for character in ['远', '控', 'A', '7'] {
            let glyph = rasterize(character, 16);
            assert!(!glyph.alpha.is_empty(), "missing glyph {character}");
            assert!(
                glyph.alpha.iter().any(|alpha| *alpha > 0),
                "blank glyph {character}"
            );
        }
    }

    #[test]
    fn truncation_fits_and_preserves_short_text() {
        assert_eq!(truncate("连接状态", 14, 400.0), "连接状态");
        let long = truncate("一条很长的中继服务器名称", 14, 60.0);
        assert!(long.ends_with('…'));
        assert!(text_width(&long, 14) <= 60.0);
    }

    #[test]
    fn drawing_clips_to_buffer_without_panicking() {
        let mut buffer = vec![0; 64 * 64];
        draw(
            &mut buffer,
            64,
            64,
            Rect {
                x: 0,
                y: 0,
                width: 24,
                height: 24,
            },
            "大肥鱼",
            16,
            0xffffff,
        );
        assert!(buffer.iter().any(|pixel| *pixel != 0));
    }
}
