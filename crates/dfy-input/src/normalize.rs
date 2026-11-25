// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 绝对坐标归一化（纯函数，便于脱离真实注入测试）。

/// 把像素坐标归一化到 Windows `SendInput` 绝对鼠标使用的 `0..=65535` 区间。
///
/// 分母使用 `extent - 1`，保证画面右下角映射到 `65535`（而不是提前一格）。
/// 尺寸为 0 或 1 时视为退化情形，统一返回 0。
pub(crate) fn normalize_abs(x: u32, y: u32, width: u32, height: u32) -> (i32, i32) {
    fn axis(value: u32, extent: u32) -> i32 {
        if extent <= 1 {
            return 0;
        }
        let normalized = u64::from(value) * 65_535 / u64::from(extent - 1);
        normalized.min(65_535) as i32
    }
    (axis(x, width), axis(y, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corners_map_to_full_range() {
        assert_eq!(normalize_abs(0, 0, 1920, 1080), (0, 0));
        assert_eq!(normalize_abs(1919, 1079, 1920, 1080), (65535, 65535));
    }

    #[test]
    fn out_of_bounds_is_clamped() {
        assert_eq!(normalize_abs(9999, 9999, 1920, 1080), (65535, 65535));
    }

    #[test]
    fn degenerate_extent_is_zero() {
        assert_eq!(normalize_abs(10, 20, 1, 0), (0, 0));
    }
}
