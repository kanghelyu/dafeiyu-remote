// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 滚轮换算：winit 的 [`MouseScrollDelta`] → 协议里的 `Wheel { dx, dy }`。
//!
//! 协议（`dfy-input::InputEvent::Wheel`）的单位是 Windows `WHEEL_DELTA`：
//! **`120` = 一格**，`dy` 正值向上、`dx` 正值向右。
//! winit 的符号约定正好相反（正值表示「内容向右/向下移动」），所以这里要取反。
//!
//! 触摸板给的通常是 `PixelDelta`（像素），按 [`PIXELS_PER_LINE`] 折算成「行」，
//! 余下的不足一行的部分留在 [`WheelAccumulator`] 里，等后续事件凑整——
//! 否则慢速滑动会被全部丢弃（< 40px 一律为 0）。

use winit::event::MouseScrollDelta;

/// Windows `WHEEL_DELTA`：一格滚轮。
pub const WHEEL_DELTA: i16 = 120;

/// 触摸板像素 → 滚轮行的折算比例（经验值，40px ≈ 一行）。
pub const PIXELS_PER_LINE: f32 = 40.0;

/// 触摸板不足一行的余量；跨事件累计，避免慢速滑动被吞掉。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct WheelAccumulator {
    /// 水平方向余量（单位：行，正值表示内容向右）。
    pub x: f32,
    /// 垂直方向余量（单位：行，正值表示内容向下）。
    pub y: f32,
}

/// 把一次滚轮事件换算成协议增量；返回 `(dx, dy)`（单位 `WHEEL_DELTA`，dy 正值向上）。
///
/// 行/像素增量都先折成「行」再累计到 [`WheelAccumulator`]，只发出整行对应的
/// `WHEEL_DELTA` 整数倍，余量留到后续事件——这样高精度滚轮/触摸板的慢速滑动不会丢事件。
/// 结果为 0 时调用方不必发事件。
#[must_use]
pub fn scroll_to_wheel(delta: MouseScrollDelta, accum: &mut WheelAccumulator) -> (i16, i16) {
    match delta {
        MouseScrollDelta::LineDelta(x, y) => {
            if !x.is_finite() || !y.is_finite() {
                // NaN/Inf 会把累加器永久污染，直接丢弃这次事件并复位。
                *accum = WheelAccumulator::default();
                return (0, 0);
            }
            // 行事件是离散格：先清掉像素余量，避免之后莫名多跳一格。
            *accum = WheelAccumulator { x, y };
            whole_lines(accum)
        }
        MouseScrollDelta::PixelDelta(pos) => {
            let (dx, dy) = (pos.x as f32, pos.y as f32);
            if !dx.is_finite() || !dy.is_finite() {
                return (0, 0);
            }
            accum.x += dx / PIXELS_PER_LINE;
            accum.y += dy / PIXELS_PER_LINE;
            whole_lines(accum)
        }
    }
}

/// 取出累加器里的整行数（余量留在累加器里留到下次），返回协议增量。
fn whole_lines(accum: &mut WheelAccumulator) -> (i16, i16) {
    let whole_x = accum.x.trunc();
    let whole_y = accum.y.trunc();
    accum.x -= whole_x;
    accum.y -= whole_y;
    (lines_to_wheel(whole_x), lines_to_wheel(whole_y))
}

/// 「行」→ 协议增量：取反（winit 正值向下、协议正值向上）、乘 120、饱和。
///
/// 饱和到 `±i16::MAX`（而不是 `i16::MIN`）：`-i16::MIN` 会溢出，注入端取反时不安全。
fn lines_to_wheel(lines: f32) -> i16 {
    let units = -lines * f32::from(WHEEL_DELTA);
    if !units.is_finite() {
        return 0;
    }
    let limit = f32::from(i16::MAX);
    units.round().clamp(-limit, limit) as i16
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::dpi::PhysicalPosition;

    #[test]
    fn line_delta_is_inverted_and_scaled_to_120() {
        let mut accum = WheelAccumulator::default();
        // 向下滚一格（winit 正值）→ 协议 dy = -120。
        assert_eq!(
            scroll_to_wheel(MouseScrollDelta::LineDelta(0.0, 1.0), &mut accum),
            (0, -120)
        );
        // 向上滚一格 → 协议 dy = +120。
        assert_eq!(
            scroll_to_wheel(MouseScrollDelta::LineDelta(0.0, -1.0), &mut accum),
            (0, 120)
        );
        // 水平：向右滚 → 协议 dx = -120（winit 正值向右，协议正值向右，这里同为取反）。
        assert_eq!(
            scroll_to_wheel(MouseScrollDelta::LineDelta(1.0, 0.0), &mut accum),
            (-120, 0)
        );
        assert_eq!(
            scroll_to_wheel(MouseScrollDelta::LineDelta(-1.0, 0.0), &mut accum),
            (120, 0)
        );
    }

    #[test]
    fn tiny_line_delta_rounds_to_zero() {
        let mut accum = WheelAccumulator::default();
        assert_eq!(
            scroll_to_wheel(MouseScrollDelta::LineDelta(0.0, 0.2), &mut accum),
            (0, 0)
        );
    }

    #[test]
    fn pixel_delta_accumulates_until_a_full_line() {
        let mut accum = WheelAccumulator::default();
        // 15px + 15px = 30px < 40px：一次 0，第二次仍然是 0（0.75 行）。
        assert_eq!(
            scroll_to_wheel(
                MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, 15.0)),
                &mut accum
            ),
            (0, 0)
        );
        assert_eq!(
            scroll_to_wheel(
                MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, 15.0)),
                &mut accum
            ),
            (0, 0)
        );
        // 再来 15px → 累计 1.125 行，发出整 1 行，余 0.125 行。
        assert_eq!(
            scroll_to_wheel(
                MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, 15.0)),
                &mut accum
            ),
            (0, -120)
        );
        assert!((accum.y - 0.125).abs() < 1e-6);
    }

    #[test]
    fn one_full_line_of_pixels_is_one_notch() {
        let mut accum = WheelAccumulator::default();
        assert_eq!(
            scroll_to_wheel(
                MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, 40.0)),
                &mut accum
            ),
            (0, -120)
        );
        // 反向
        assert_eq!(
            scroll_to_wheel(
                MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, -40.0)),
                &mut accum
            ),
            (0, 120)
        );
    }

    #[test]
    fn huge_delta_saturates_instead_of_overflowing() {
        let mut accum = WheelAccumulator::default();
        assert_eq!(
            scroll_to_wheel(MouseScrollDelta::LineDelta(0.0, 10_000.0), &mut accum),
            (0, i16::MIN + 1)
        );
        assert_eq!(
            scroll_to_wheel(MouseScrollDelta::LineDelta(0.0, -10_000.0), &mut accum),
            (0, i16::MAX)
        );
        // NaN 不能变成 0 以外的怪值。
        assert_eq!(
            scroll_to_wheel(MouseScrollDelta::LineDelta(0.0, f32::NAN), &mut accum),
            (0, 0)
        );
    }

    #[test]
    fn line_event_clears_pixel_remainder() {
        let mut accum = WheelAccumulator::default();
        let _ = scroll_to_wheel(
            MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, 30.0)),
            &mut accum,
        );
        assert!(accum.y > 0.0);
        let _ = scroll_to_wheel(MouseScrollDelta::LineDelta(0.0, 1.0), &mut accum);
        assert_eq!(accum, WheelAccumulator::default());
    }
}
