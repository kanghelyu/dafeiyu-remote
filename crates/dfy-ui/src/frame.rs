// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 帧格式转换与等比缩放。
//!
//! 这里全是**纯函数**，不碰窗口/事件循环，便于单测和未来复用到 GPU 纹理上传路径。
//!
//! # 像素格式
//!
//! - 输入：`dfy-codec` 解码出来的 BGRA，内存字节序为 `B, G, R, A`（即
//!   `0xAARRGGBB` 在 little-endian 上的内存布局）。
//! - 输出：softbuffer 的 32 位像素，格式固定为 `0x00RRGGBB`
//!   （最高 8 位必须为 0，随后是 R、G、B；见 softbuffer 文档 "Data representation"）。
//!
//! 所以转换只做通道重排，**不做** premultiply / gamma 处理，alpha 直接丢弃
//! （桌面画面本来是不透明的，`dfy-capture` 采集时 alpha 恒为 `0xFF`）。

/// 一帧**已校验**的远端画面（BGRA8，内存字节序 `B, G, R, A`）。
///
/// 构造时就把形状钉死：宽高非 0、`bgra.len() == width * height * 4`。因此拿到
/// `Frame` 的代码不必再重复校验，渲染/推帧路径可以把它当作不变式使用。
///
/// 输入来自 `dfy-session::DecodedFrame`（BGRA8，可能带行跨度 padding）时，先用
/// [`Frame::from_strided`] 复制成紧凑布局，再交给 [`crate::run_viewer`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    width: u32,
    height: u32,
    bgra: Vec<u8>,
}

/// [`Frame::new`] / [`Frame::from_strided`] 的校验错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// 宽或高为 0。
    ZeroSize {
        /// 传入的宽。
        width: u32,
        /// 传入的高。
        height: u32,
    },
    /// `width * height * 4` 超出 `usize` 可表示范围（病态尺寸，防溢出）。
    TooLarge {
        /// 传入的宽。
        width: u32,
        /// 传入的高。
        height: u32,
    },
    /// 数据长度与宽高不符。
    LengthMismatch {
        /// 期望字节数（`width * height * 4`，或 strided 布局的最后一行末尾）。
        expected: usize,
        /// 实际字节数。
        actual: usize,
    },
    /// 行跨度小于一行 BGRA 的字节数（`width * 4`）。
    StrideTooSmall {
        /// 传入的行跨度。
        stride: usize,
        /// 一行 BGRA 至少需要的字节数。
        row_bytes: usize,
    },
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::ZeroSize { width, height } => {
                write!(f, "帧尺寸不能为 0（{width}×{height}）")
            }
            FrameError::TooLarge { width, height } => {
                write!(f, "帧尺寸过大，字节数溢出（{width}×{height}）")
            }
            FrameError::LengthMismatch { expected, actual } => {
                write!(
                    f,
                    "帧数据长度不符：期望 {expected} 字节，实际 {actual} 字节"
                )
            }
            FrameError::StrideTooSmall { stride, row_bytes } => {
                write!(f, "行跨度 {stride} 小于一行所需的 {row_bytes} 字节")
            }
        }
    }
}

impl std::error::Error for FrameError {}

impl Frame {
    /// 校验并构造一帧紧凑 BGRA。
    ///
    /// # Errors
    ///
    /// 宽/高为 0、`bgra.len() != width * height * 4`、或尺寸大到 `usize` 溢出时返回错误。
    pub fn new(width: u32, height: u32, bgra: Vec<u8>) -> Result<Self, FrameError> {
        if width == 0 || height == 0 {
            return Err(FrameError::ZeroSize { width, height });
        }
        let expected = (width as usize)
            .checked_mul(height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or(FrameError::TooLarge { width, height })?;
        if bgra.len() != expected {
            return Err(FrameError::LengthMismatch {
                expected,
                actual: bgra.len(),
            });
        }
        Ok(Self {
            width,
            height,
            bgra,
        })
    }

    /// 从「带行跨度」的解码缓冲复制出一帧紧凑 BGRA（输出长度恒为 `width * height * 4`）。
    ///
    /// 解码器（`dfy-session::DecodedFrame`）与大尺寸采集常用「每行按 4 字节对齐/按 256
    /// 字节对齐」的布局：`data` 长度是 `stride * height`，而 `stride >= width * 4`。
    /// softbuffer 只接受紧凑像素缓冲，所以在这里做一次行拷贝，把 padding 丢掉。
    ///
    /// # Errors
    ///
    /// 宽/高为 0、`stride < width * 4`、`data` 不足以容纳最后一行、或尺寸溢出时返回错误。
    pub fn from_strided(
        width: u32,
        height: u32,
        stride: usize,
        data: &[u8],
    ) -> Result<Self, FrameError> {
        if width == 0 || height == 0 {
            return Err(FrameError::ZeroSize { width, height });
        }
        let row_bytes = (width as usize)
            .checked_mul(4)
            .ok_or(FrameError::TooLarge { width, height })?;
        if stride < row_bytes {
            return Err(FrameError::StrideTooSmall { stride, row_bytes });
        }
        let last_row_start = (height as usize - 1)
            .checked_mul(stride)
            .ok_or(FrameError::TooLarge { width, height })?;
        let expected = last_row_start
            .checked_add(row_bytes)
            .ok_or(FrameError::TooLarge { width, height })?;
        if data.len() < expected {
            return Err(FrameError::LengthMismatch {
                expected,
                actual: data.len(),
            });
        }
        let mut bgra = Vec::with_capacity(expected);
        for row in 0..height as usize {
            let start = row * stride;
            bgra.extend_from_slice(&data[start..start + row_bytes]);
        }
        Ok(Self {
            width,
            height,
            bgra,
        })
    }

    /// 画面宽（像素）。
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// 画面高（像素）。
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// 画面尺寸 `(width, height)`。
    #[must_use]
    pub const fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// 紧凑 BGRA 数据（长度恒为 `width * height * 4`）。
    #[must_use]
    pub fn bgra(&self) -> &[u8] {
        &self.bgra
    }

    /// 取走 BGRA 数据（避免推帧路径再复制一次）。
    #[must_use]
    pub fn into_bgra(self) -> Vec<u8> {
        self.bgra
    }

    /// 字节数（= `width * height * 4`）。
    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.bgra.len()
    }
}

/// 窗口物理像素坐标系里的一块矩形。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    /// 左边界（物理像素）。
    pub x: u32,
    /// 上边界（物理像素）。
    pub y: u32,
    /// 宽度（物理像素，至少 1）。
    pub width: u32,
    /// 高度（物理像素，至少 1）。
    pub height: u32,
}

/// 把 4 个 BGRA 字节转成 softbuffer 的 `0x00RRGGBB`。
#[inline]
#[must_use]
pub fn bgra_to_pixel(bgra: [u8; 4]) -> u32 {
    let [b, g, r, _a] = bgra;
    ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

/// 从切片里取 4 个 BGRA 字节转成一个像素；调用方保证切片长度足够。
#[inline]
fn pixel_at(src: &[u8], offset: usize) -> u32 {
    // 索引已由调用方（render_frame 的长度校验）保证合法。
    let b = src[offset];
    let g = src[offset + 1];
    let r = src[offset + 2];
    ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

/// 整帧 1:1 BGRA → softbuffer 像素（不缩放）。
///
/// 只用于测试与「未来 GPU 路径的 CPU 基线」，每次调用都会分配一个新 `Vec`；
/// 真正的渲染走 [`render_frame`]，直接写进 softbuffer 的缓冲区，不额外分配。
#[must_use]
pub fn bgra_to_pixels(bgra: &[u8]) -> Vec<u32> {
    bgra.chunks_exact(4)
        .map(|c| bgra_to_pixel([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// 计算等比（letterbox）缩放后画面在窗口里的位置。
///
/// - 返回 `None`：任一尺寸为 0（窗口最小化等），调用方应跳过渲染。
/// - 缩放比例取 `min(dst_w/src_w, dst_h/src_h)`，用整数交叉相乘比较，避免浮点误差。
/// - 输出宽高四舍五入到整数像素，并夹在 `1..=dst` 内；因此可能出现 1px 的不对称黑边。
#[must_use]
pub fn fit_rect(src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Option<Rect> {
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return None;
    }
    let (sw, sh) = (u64::from(src_w), u64::from(src_h));
    let (dw, dh) = (u64::from(dst_w), u64::from(dst_h));
    // scale = num / den，且 num <= den（等比缩小/放大到贴边）。
    let (num, den) = if dw * sh <= dh * sw {
        (dw, sw)
    } else {
        (dh, sh)
    };
    // 四舍五入：x/den 的整数近似 (x + den/2) / den。
    let round = |v: u64| (v * num + den / 2) / den;
    let out_w = round(sw).clamp(1, dw);
    let out_h = round(sh).clamp(1, dh);
    Some(Rect {
        x: ((dw - out_w) / 2) as u32,
        y: ((dh - out_h) / 2) as u32,
        width: out_w as u32,
        height: out_h as u32,
    })
}

/// 渲染一帧：把 BGRA 源按等比最近邻缩放写进整块窗口缓冲区（其余区域填黑）。
///
/// - `dst` 必须是长度恰好为 `dst_w * dst_h` 的 softbuffer 像素缓冲。
/// - 返回 `false` 表示参数不合法（尺寸/长度不匹配），调用方应打日志并跳过本帧。
/// - 最近邻：先算一列 `x` 映射表，再逐行搬；不分配额外缓冲（除映射表）。
#[must_use]
pub fn render_frame(
    src: &[u8],
    src_w: u32,
    src_h: u32,
    dst: &mut [u32],
    dst_w: u32,
    dst_h: u32,
) -> bool {
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return false;
    }
    let src_len = src_w as usize * src_h as usize * 4;
    let dst_len = dst_w as usize * dst_h as usize;
    if src.len() != src_len || dst.len() != dst_len {
        return false;
    }
    // letterbox 区域是黑的。
    dst.fill(0);
    let Some(rect) = fit_rect(src_w, src_h, dst_w, dst_h) else {
        return false;
    };
    // x 方向映射表：目标列 -> 源列。
    let xmap: Vec<usize> = (0..rect.width as u64)
        .map(|x| (x * u64::from(src_w) / u64::from(rect.width)) as usize)
        .collect();
    let src_row_pixels = src_w as usize;
    for row in 0..rect.height {
        let src_y = (u64::from(row) * u64::from(src_h) / u64::from(rect.height)) as usize;
        let src_row = &src[src_y * src_row_pixels * 4..];
        let dst_offset = ((rect.y + row) * dst_w + rect.x) as usize;
        let dst_row = &mut dst[dst_offset..dst_offset + rect.width as usize];
        for (dx, &src_x) in xmap.iter().enumerate() {
            dst_row[dx] = pixel_at(src_row, src_x * 4);
        }
    }
    true
}

/// 把窗口内的光标位置（物理像素）反解成远端画面像素坐标。
///
/// - 落在外框黑边上时**夹到画面边缘**（而不是丢弃事件），避免远端鼠标卡住。
/// - `dst_w/dst_h` 是窗口客户区尺寸，`src_w/src_h` 是远端画面尺寸。
/// - 返回 `None`：任一尺寸为 0。
#[must_use]
pub fn cursor_to_frame(
    x: f64,
    y: f64,
    dst_w: u32,
    dst_h: u32,
    src_w: u32,
    src_h: u32,
) -> Option<(u32, u32)> {
    let rect = fit_rect(src_w, src_h, dst_w, dst_h)?;
    let rel_x = ((x.round() as i64) - i64::from(rect.x)).clamp(0, i64::from(rect.width) - 1);
    let rel_y = ((y.round() as i64) - i64::from(rect.y)).clamp(0, i64::from(rect.height) - 1);
    let px = (rel_x as u64 * u64::from(src_w) / u64::from(rect.width)).min(u64::from(src_w) - 1);
    let py = (rel_y as u64 * u64::from(src_h) / u64::from(rect.height)).min(u64::from(src_h) - 1);
    Some((px as u32, py as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bgra_byte_order_becomes_00rrggbb() {
        // B=0x11 G=0x22 R=0x33 A=0xFF -> 0x00332211
        assert_eq!(bgra_to_pixel([0x11, 0x22, 0x33, 0xFF]), 0x0033_2211);
        assert_eq!(bgra_to_pixel([0xFF, 0x00, 0x00, 0xFF]), 0x0000_00FF);
        assert_eq!(bgra_to_pixel([0x00, 0x00, 0xFF, 0xFF]), 0x00FF_0000);
        // alpha 必须被丢掉（最高 8 位恒 0）。
        assert_eq!(bgra_to_pixel([0x11, 0x22, 0x33, 0x00]), 0x0033_2211);
    }

    #[test]
    fn bgra_frame_conversion_is_1to1() {
        let bytes = [1, 2, 3, 4, 5, 6, 7, 8];
        assert_eq!(bgra_to_pixels(&bytes), vec![0x0003_0201, 0x0007_0605]);
        // 非 4 的倍数尾部被忽略。
        assert_eq!(bgra_to_pixels(&[1, 2, 3, 4, 9]), vec![0x0003_0201]);
    }

    #[test]
    fn fit_rect_exact_aspect_ratio_fills_window() {
        // 同比例放大：1280x720 -> 1920x1080，无黑边。
        assert_eq!(
            fit_rect(1280, 720, 1920, 1080),
            Some(Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080
            })
        );
        // 同比例缩小：4x4 -> 2x2。
        assert_eq!(
            fit_rect(4, 4, 2, 2),
            Some(Rect {
                x: 0,
                y: 0,
                width: 2,
                height: 2
            })
        );
    }

    #[test]
    fn fit_rect_letterboxes_wide_window() {
        // 1280x720 放进 1000x1000 的方窗：宽度贴边，上下留黑边。
        assert_eq!(
            fit_rect(1280, 720, 1000, 1000),
            Some(Rect {
                x: 0,
                y: 218,
                width: 1000,
                height: 563
            })
        );
    }

    #[test]
    fn fit_rect_letterboxes_tall_window() {
        // 1000x1000 的方画面放进 800x600 的横窗：高度贴边，左右留黑边。
        assert_eq!(
            fit_rect(1000, 1000, 800, 600),
            Some(Rect {
                x: 100,
                y: 0,
                width: 600,
                height: 600
            })
        );
    }

    #[test]
    fn fit_rect_rejects_zero_then_never_exceeds_window() {
        assert_eq!(fit_rect(0, 720, 100, 100), None);
        assert_eq!(fit_rect(1280, 0, 100, 100), None);
        assert_eq!(fit_rect(1280, 720, 0, 100), None);
        assert_eq!(fit_rect(1280, 720, 100, 0), None);
        for (src_w, src_h) in [(1u32, 1000u32), (1000, 1), (1, 1), (3840, 2160), (17, 33)] {
            for (dst_w, dst_h) in [(1u32, 1u32), (7, 3), (64, 64), (1920, 1080)] {
                let rect = fit_rect(src_w, src_h, dst_w, dst_h).expect("非零尺寸必须有结果");
                assert!(rect.width >= 1 && rect.height >= 1);
                assert!(rect.width <= dst_w && rect.height <= dst_h);
                assert!(rect.x + rect.width <= dst_w);
                assert!(rect.y + rect.height <= dst_h);
            }
        }
    }

    #[test]
    fn render_frame_nearest_neighbour_2x2_to_4x4() {
        // 4 个像素：红 绿 / 蓝 白（BGRA 字节）。
        let src: Vec<u8> = vec![
            0x00, 0x00, 0xFF, 0xFF, // 红
            0x00, 0xFF, 0x00, 0xFF, // 绿
            0xFF, 0x00, 0x00, 0xFF, // 蓝
            0xFF, 0xFF, 0xFF, 0xFF, // 白
        ];
        let mut dst = vec![0xDEAD_BEEFu32; 4 * 4];
        assert!(render_frame(&src, 2, 2, &mut dst, 4, 4));
        let red = 0x00FF_0000;
        let green = 0x0000_FF00;
        let blue = 0x0000_00FF;
        let white = 0x00FF_FFFF;
        assert_eq!(
            dst,
            vec![
                red, red, green, green, //
                red, red, green, green, //
                blue, blue, white, white, //
                blue, blue, white, white, //
            ]
        );
    }

    #[test]
    fn render_frame_fills_letterbox_black() {
        // 2x2 源画面放进 4x2 窗口：等比后居中 2 列，两侧留 1 列黑边（残留值必须被清掉）。
        let src: Vec<u8> = vec![
            0x01, 0x02, 0x03, 0xFF, 0x04, 0x05, 0x06, 0xFF, //
            0x07, 0x08, 0x09, 0xFF, 0x0A, 0x0B, 0x0C, 0xFF,
        ];
        let mut dst = vec![0xFFFF_FFFFu32; 4 * 2];
        assert!(render_frame(&src, 2, 2, &mut dst, 4, 2));
        // 第一行：黑、源(0,0)、源(0,1)、黑
        assert_eq!(dst[0], 0x0000_0000);
        assert_eq!(dst[1], 0x0003_0201);
        assert_eq!(dst[2], 0x0006_0504);
        assert_eq!(dst[3], 0x0000_0000);
        // 第二行：黑、源(1,0)、源(1,1)、黑
        assert_eq!(dst[4], 0x0000_0000);
        assert_eq!(dst[5], 0x0009_0807);
        assert_eq!(dst[6], 0x000C_0B0A);
        assert_eq!(dst[7], 0x0000_0000);
    }

    #[test]
    fn render_frame_rejects_bad_shapes() {
        let src = vec![0u8; 4 * 4 * 4];
        let mut dst = vec![0u32; 4 * 4];
        // 源宽高与数据不符
        assert!(!render_frame(&src, 3, 3, &mut dst, 4, 4));
        // 目标缓冲长度不符
        let mut small = vec![0u32; 3];
        assert!(!render_frame(&src, 2, 2, &mut small, 4, 4));
        // 零尺寸
        let mut none = Vec::<u32>::new();
        assert!(!render_frame(&src, 0, 2, &mut none, 0, 0));
    }

    #[test]
    fn cursor_maps_back_through_letterbox() {
        // 1280x720 画面显示在 1000x1000 窗口里（rect = y 218..781）。
        // 窗口正中 -> 画面正中。
        assert_eq!(
            cursor_to_frame(500.0, 500.0, 1000, 1000, 1280, 720),
            Some((640, 360))
        );
        // y=100 落在上方黑边里 -> y 夹到 0；x=1 已在画面内，按比例落到源第 1 列。
        assert_eq!(
            cursor_to_frame(1.0, 100.0, 1000, 1000, 1280, 720),
            Some((1, 0))
        );
        // 右下角黑边 -> 夹到画面最后一行/列的最近邻采样点（563 行显示 720 行，最后一行对应 718）
        assert_eq!(
            cursor_to_frame(999.0, 999.0, 1000, 1000, 1280, 720),
            Some((1278, 718))
        );
        // 画面左边缘第一个像素
        assert_eq!(
            cursor_to_frame(0.0, 218.0, 1000, 1000, 1280, 720),
            Some((0, 0))
        );
    }

    #[test]
    fn frame_new_accepts_exact_bgra() {
        let frame = Frame::new(2, 3, vec![0xAB; 2 * 3 * 4]).expect("合法帧必须构造成功");
        assert_eq!(frame.width(), 2);
        assert_eq!(frame.height(), 3);
        assert_eq!(frame.size(), (2, 3));
        assert_eq!(frame.byte_len(), 24);
        assert_eq!(frame.bgra().len(), 24);
        assert_eq!(frame.into_bgra().len(), 24);
    }

    #[test]
    fn frame_new_rejects_zero_size() {
        assert_eq!(
            Frame::new(0, 1, Vec::new()),
            Err(FrameError::ZeroSize {
                width: 0,
                height: 1
            })
        );
        assert_eq!(
            Frame::new(1, 0, Vec::new()),
            Err(FrameError::ZeroSize {
                width: 1,
                height: 0
            })
        );
    }

    #[test]
    fn frame_new_rejects_length_mismatch() {
        assert_eq!(
            Frame::new(2, 2, vec![0; 15]),
            Err(FrameError::LengthMismatch {
                expected: 16,
                actual: 15
            })
        );
        assert_eq!(
            Frame::new(2, 2, vec![0; 17]),
            Err(FrameError::LengthMismatch {
                expected: 16,
                actual: 17
            })
        );
    }

    #[test]
    fn frame_new_rejects_byte_length_overflow() {
        // u32::MAX × u32::MAX 像素再乘 4 字节会超出 usize，必须报 TooLarge 而不是溢出 panic。
        assert_eq!(
            Frame::new(u32::MAX, u32::MAX, Vec::new()),
            Err(FrameError::TooLarge {
                width: u32::MAX,
                height: u32::MAX
            })
        );
    }

    #[test]
    fn frame_from_strided_strips_row_padding() {
        // 2x2 BGRA，行跨度 12（每行多 4 字节 padding），padding 里塞垃圾值。
        let data = vec![
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0xEE, 0xEE, 0xEE,
            0xEE, // 第 0 行
            0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0xEE, 0xEE, 0xEE,
            0xEE, // 第 1 行
        ];
        let frame = Frame::from_strided(2, 2, 12, &data).expect("带跨度帧必须构造成功");
        assert_eq!(frame.size(), (2, 2));
        assert_eq!(
            frame.bgra(),
            &[
                0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16,
                0x17, 0x18,
            ],
            "padding 必须被丢掉"
        );
    }

    #[test]
    fn frame_from_strided_rejects_bad_layout() {
        assert_eq!(
            Frame::from_strided(2, 2, 6, &[0; 24]),
            Err(FrameError::StrideTooSmall {
                stride: 6,
                row_bytes: 8
            })
        );
        // 行跨度合法，但最后一行不完整：第 1 行从 offset 12 开始，还差 1 字节。
        assert_eq!(
            Frame::from_strided(2, 2, 12, &[0; 19]),
            Err(FrameError::LengthMismatch {
                expected: 20,
                actual: 19
            })
        );
        assert_eq!(
            Frame::from_strided(0, 2, 8, &[]),
            Err(FrameError::ZeroSize {
                width: 0,
                height: 2
            })
        );
    }
}
