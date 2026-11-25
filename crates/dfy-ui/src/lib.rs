// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Dafeiyu Remote contributors

//! dfy-ui —— 大肥鱼远程（Dafeiyu Remote）主控端**上屏组件**。
//!
//! 本 crate 只做三件事：
//!
//! 1. 把一帧 BGRA 远端画面显示到窗口里（等比最近邻缩放 + letterbox 黑边）；
//! 2. 把窗口的键盘/鼠标事件翻译成与 `dfy-input`/DFYP 输入事件同语义的 [`ViewerEvent`]；
//! 3. 提供可替换的帧来源抽象 [`FrameSource`]，让 CLI 的 `view` 子命令能把解码帧直接
//!    灌进窗口，而不用关心 winit 线程模型。
//!
//! 它**不**持有网络、会话、编解码状态，也不依赖 `dfy-session`/`dfy-transport`，
//! 只依赖 `winit` + `softbuffer`（窗口与软件上屏）+ `tokio` 的 `sync`（事件通道）。
//! 线程模型、像素格式、按键转发策略、CLI 集成方式见 `design/026-viewer-ui.md`。
//!
//! # 两个入口
//!
//! - [`spawn_viewer`]：UI 跑在独立线程，返回 [`ViewerHandle`]（异步收事件）+
//!   [`PresenterHandle`]（跨线程推帧）。适合主控端「边收边显示 + 业务线程处理输入」的常驻模式。
//! - [`run_viewer`]：**阻塞**当前线程，直到窗口关闭。适合 CLI `view` 子命令：
//!   解码循环把帧送进 channel，channel 实现 [`FrameSource`]，`run_viewer` 负责起窗/渲染/收输入。
//!
//! # 快速上手
//!
//! 常驻模式（推帧 + 收事件）：
//!
//! ```no_run
//! use dfy_ui::{spawn_viewer, ViewerConfig, ViewerEvent};
//!
//! # fn main() -> anyhow::Result<()> {
//! let mut viewer = spawn_viewer(ViewerConfig::default())?;
//! // 网络/解码线程：每解出一帧就 present（可跨线程；旧帧会被丢弃，只留最新一帧）。
//! viewer.present.present(2, 2, vec![0u8; 2 * 2 * 4]);
//! while let Some(event) = viewer.event_rx.blocking_recv() {
//!     match event {
//!         ViewerEvent::Closed => break,
//!         other => { let _ = other; }
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! 阻塞模式（`run_viewer`，CLI `view` 子命令的集成形态）：
//!
//! ```no_run
//! use dfy_ui::{run_viewer, FramePoll, FrameSource, ViewerConfig};
//!
//! struct NoFrames;
//! impl FrameSource for NoFrames {
//!     fn try_next_frame(&mut self) -> FramePoll {
//!         FramePoll::Closed
//!     }
//! }
//!
//! # fn main() -> anyhow::Result<()> {
//! run_viewer(ViewerConfig::default(), NoFrames)?;
//! # Ok(())
//! # }
//! ```
//!
//! # Feature
//!
//! - `window`（默认开启）：窗口 + softbuffer 软件上屏（[`spawn_viewer`]/[`run_viewer`]）。
//! - `--no-default-features`：退化成纯逻辑库，帧校验/缩放、键位映射、滚轮换算、
//!   渲染统计、帧来源抽象全部照常编译并可单测，不需要显示环境。
//!
//! # 线程模型
//!
//! winit 事件循环只能在特定线程创建/驱动（Windows/Linux 任意线程，macOS 要求主线程）。
//! [`spawn_viewer`] 用独立 OS 线程（`dfy-viewer-ui`）跑事件循环；[`run_viewer`] 在**调用线程**
//! 上跑，因此 CLI 应在主线程调用它（不要塞进 tokio 的 `spawn_blocking` 之外的随机 worker）。
//!
//! 跨线程只有两条路：推帧走 [`PresenterHandle`]（单槽最新帧邮箱 + 唤醒），
//! 收事件走 [`ViewerHandle::event_rx`]（tokio unbounded mpsc，顺序投递、不阻塞 UI 线程）。

mod frame;
mod keymap;
mod scroll;
mod source;
mod stats;

#[cfg(feature = "window")]
mod app;
#[cfg(feature = "window")]
mod mailbox;
#[cfg(feature = "window")]
mod run;

pub use frame::{
    bgra_to_pixel, bgra_to_pixels, cursor_to_frame, fit_rect, render_frame, Frame, FrameError, Rect,
};
pub use keymap::{hid_button, hid_usage, modifier_bit};
pub use scroll::{scroll_to_wheel, WheelAccumulator, PIXELS_PER_LINE, WHEEL_DELTA};
pub use source::{FrameIter, FramePoll, FrameSource, DEFAULT_POLL_INTERVAL};
pub use stats::{RenderStats, FPS_WINDOW, TITLE_REFRESH_INTERVAL};

#[cfg(feature = "window")]
pub use app::spawn_viewer;
#[cfg(feature = "window")]
pub use run::{run_viewer, run_viewer_with};

#[cfg(feature = "window")]
use std::sync::Arc;

#[cfg(feature = "window")]
use winit::event_loop::EventLoopProxy;

#[cfg(feature = "window")]
use crate::frame::Frame as PresentableFrame;
#[cfg(feature = "window")]
use crate::mailbox::{FrameMailbox, UiCommand};

/// 主控端上屏窗口的配置。
///
/// 全部字段都是 `pub`，可以直接用结构体字面量构造；也可以从 [`ViewerConfig::default`] 起步链式覆盖。
///
/// ```
/// use dfy_ui::ViewerConfig;
///
/// let config = ViewerConfig::new("演示窗口")
///     .with_initial_size(1920, 1080)
///     .with_fullscreen(true)
///     .with_stats(true);
/// assert_eq!(config.initial_size, (1920, 1080));
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct ViewerConfig {
    /// 窗口标题；开启统计时会在基础标题后追加 FPS/耗时（由 UI 线程节流刷新）。
    pub title: String,
    /// 窗口初始客户区尺寸（物理像素），默认 `(1280, 720)`。
    ///
    /// 每个分量独立处理：`0` 表示该分量取默认值，其余夹到
    /// `ViewerConfig::MIN_INITIAL_SIZE..=ViewerConfig::MAX_INITIAL_SIZE`（宽高都受限，
    /// 防止首帧异常尺寸撑爆窗口）。
    pub initial_size: (u32, u32),
    /// 启动即无边框全屏（`Fullscreen::Borderless`）；`F11` 随时切换。
    pub fullscreen: bool,
    /// 是否在窗口标题里显示「画面尺寸 · FPS · 单帧渲染耗时」。
    ///
    /// 关闭（默认）时标题保持 [`ViewerConfig::title`] 不变；开启时每 500ms 最多刷新一次。
    pub show_stats: bool,
    /// 首帧到达时按「首帧尺寸 × `scale`」把窗口调整一次；`None` 表示**不**按帧调整，
    /// 窗口保持 [`ViewerConfig::initial_size`]。
    ///
    /// `Some` 值会夹到 `0.25..=4.0`（`NaN`/`inf` 视为 `None`）。之后窗口尺寸只由用户拖拽/
    /// 全屏决定，渲染始终等比贴合窗口。
    pub scale: Option<f64>,
}

impl ViewerConfig {
    /// 默认窗口尺寸（宽 × 高，物理像素）。
    pub const DEFAULT_INITIAL_SIZE: (u32, u32) = (1280, 720);
    /// 初始窗口尺寸下限（单边像素）。
    pub const MIN_INITIAL_SIZE: u32 = 16;
    /// 初始窗口尺寸上限（单边像素）。
    pub const MAX_INITIAL_SIZE: u32 = 16_384;

    /// 用标题构造默认配置。
    #[must_use]
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            ..Self::default()
        }
    }

    /// 设置窗口初始尺寸（物理像素）。
    #[must_use]
    pub fn with_initial_size(mut self, width: u32, height: u32) -> Self {
        self.initial_size = (width, height);
        self
    }

    /// 设置是否启动即全屏。
    #[must_use]
    pub fn with_fullscreen(mut self, fullscreen: bool) -> Self {
        self.fullscreen = fullscreen;
        self
    }

    /// 设置是否在标题栏显示统计。
    #[must_use]
    pub fn with_stats(mut self, show_stats: bool) -> Self {
        self.show_stats = show_stats;
        self
    }

    /// 设置首帧缩放倍数（`None` = 不按首帧调整窗口）。
    #[must_use]
    pub fn with_scale(mut self, scale: Option<f64>) -> Self {
        self.scale = scale;
        self
    }

    /// 实际生效的初始窗口尺寸（处理 `0` 与上下限夹取）。
    #[must_use]
    pub fn effective_initial_size(&self) -> (u32, u32) {
        let (default_width, default_height) = Self::DEFAULT_INITIAL_SIZE;
        let (mut width, mut height) = self.initial_size;
        if width == 0 {
            width = default_width;
        }
        if height == 0 {
            height = default_height;
        }
        (
            width.clamp(Self::MIN_INITIAL_SIZE, Self::MAX_INITIAL_SIZE),
            height.clamp(Self::MIN_INITIAL_SIZE, Self::MAX_INITIAL_SIZE),
        )
    }

    /// 实际生效的首帧缩放倍数（`NaN`/`inf` → `None`；其余夹到 `0.25..=4.0`）。
    #[must_use]
    pub fn effective_scale(&self) -> Option<f64> {
        self.scale
            .filter(|scale| scale.is_finite())
            .map(|scale| scale.clamp(0.25, 4.0))
    }
}

impl Default for ViewerConfig {
    fn default() -> Self {
        Self {
            title: "大肥鱼远程 · 主控端".to_string(),
            initial_size: Self::DEFAULT_INITIAL_SIZE,
            fullscreen: false,
            show_stats: false,
            scale: None,
        }
    }
}

/// 窗口事件（与 `dfy-input`/DFYP 协议语义对齐；本 crate 只翻译，不实现业务）。
///
/// 坐标/单位约定：
///
/// - [`ViewerEvent::Key`]：`usage` 是 HID Keyboard/Keypad page（`0x07`）Usage ID，
///   `modifiers` 是 HID 修饰键位图（bit0=LCtrl … bit7=RGui）。
/// - [`ViewerEvent::Mouse`]：`x`/`y` 是**远端画面像素坐标**，`width`/`height` 是画面尺寸；
///   `button` 为 `None` 表示纯移动，`Some(1..=5)` 表示按键（1=左、2=右、3=中、4=后退、5=前进）。
/// - [`ViewerEvent::Wheel`]：单位是 Windows `WHEEL_DELTA`（`120` = 一格），
///   `dy` 正值向上、`dx` 正值向右（与 `dfy-input::InputEvent::Wheel` 完全一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewerEvent {
    /// 键盘按下/抬起。
    Key {
        /// HID Usage ID。
        usage: u16,
        /// `true` = 按下，`false` = 抬起。
        down: bool,
        /// 当前修饰键位图。
        modifiers: u8,
    },
    /// 鼠标移动（`button: None`）或鼠标按键（`button: Some(1..=5)`）。
    Mouse {
        /// 远端画面像素横坐标。
        x: u32,
        /// 远端画面像素纵坐标。
        y: u32,
        /// 远端画面宽度。
        width: u32,
        /// 远端画面高度。
        height: u32,
        /// 鼠标按键（1=左、2=右、3=中、4=后退、5=前进）；`None` = 纯移动。
        button: Option<u8>,
        /// 按键事件是按下还是抬起（纯移动时固定为 `false`）。
        down: bool,
    },
    /// 滚轮（单位 `WHEEL_DELTA`，`dy` 正值向上）。
    Wheel {
        /// 水平增量。
        dx: i16,
        /// 垂直增量。
        dy: i16,
    },
    /// 窗口已关闭（用户点 X、`Esc`、帧来源结束、或调用方关闭）；这是**终态**事件。
    Closed,
    /// `Ctrl+Alt+Shift+Q`：调用方应切换「是否允许远控输入」（只发事件，不实现业务）。
    ToggleControl,
    /// `F11`：调用方收到时窗口已切换全屏（事件仅作通知）。
    ToggleFullscreen,
}

/// 事件处理结果：回调式入口（[`run_viewer_with`]）用它决定窗口是否继续运行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewerControl {
    /// 继续显示（正常返回值）。
    Continue,
    /// 请求关闭窗口并结束 viewer（等价于用户点 X）。
    Close,
}

/// 跨线程推帧句柄（`Clone`，可交给解码线程/网络线程任意持有）。
#[cfg(feature = "window")]
#[derive(Clone)]
pub struct PresenterHandle {
    mailbox: Arc<FrameMailbox>,
    proxy: EventLoopProxy<UiCommand>,
}

#[cfg(feature = "window")]
impl PresenterHandle {
    /// 提交一帧 BGRA 画面（内存字节序 `B, G, R, A`）。
    ///
    /// - 可跨线程调用；内部只保留**最新一帧**，来不及渲染的旧帧直接丢弃。
    /// - `width`/`height` 必须非 0，且 `bgra.len() == width * height * 4`，否则本帧被丢弃并打 warn。
    ///   已经构造好 [`Frame`] 时请用 [`PresenterHandle::present_frame`]（不再重复校验）。
    /// - 事件循环已退出时调用是安全的空操作。
    pub fn present(&self, width: u32, height: u32, bgra: Vec<u8>) {
        match PresentableFrame::new(width, height, bgra) {
            Ok(frame) => self.present_frame(frame),
            Err(err) => tracing::warn!(%err, "忽略非法帧"),
        }
    }

    /// 提交一个已校验的 [`Frame`]（推荐路径：解码线程直接构造 `Frame`，避免重复校验）。
    pub fn present_frame(&self, frame: PresentableFrame) {
        self.mailbox.store_frame(frame);
        if self.proxy.send_event(UiCommand::Frame).is_err() {
            tracing::debug!("viewer 事件循环已退出，忽略推帧");
        }
    }

    /// 窗口是否已经创建完成（CI/demo 用来确认「窗口真的起来了」）。
    #[must_use]
    pub fn is_window_ready(&self) -> bool {
        self.mailbox.window_ready()
    }

    /// 请求关闭窗口并结束 UI 线程（幂等）。
    pub fn close(&self) {
        if self.proxy.send_event(UiCommand::Close).is_err() {
            tracing::debug!("viewer 事件循环已退出，close 忽略");
        }
    }
}

/// 上屏窗口句柄：收窗口事件 + 推帧。
///
/// 丢弃该句柄（或调用 [`ViewerHandle::close`]）会让 UI 线程关闭窗口并退出；
/// 之后 [`PresenterHandle::present`] 变成空操作。
#[cfg(feature = "window")]
pub struct ViewerHandle {
    /// 窗口事件流；消费方丢弃它等同于「不再关心窗口」（UI 线程会在下一次事件/唤醒时退出）。
    pub event_rx: tokio::sync::mpsc::UnboundedReceiver<ViewerEvent>,
    /// 推帧句柄（可 `clone()` 给其他线程）。
    pub present: PresenterHandle,
}

#[cfg(feature = "window")]
impl ViewerHandle {
    /// 请求关闭窗口（异步语义：请求立即入队，UI 线程稍后退出）。
    pub fn close(&self) {
        self.present.close();
    }

    /// 非阻塞取一个窗口事件（供没有 tokio runtime 的同步调用方使用）。
    #[must_use]
    pub fn try_recv(&mut self) -> Option<ViewerEvent> {
        self.event_rx.try_recv().ok()
    }

    /// 异步取一个窗口事件；通道关闭（UI 线程退出）后返回 `None`。
    pub async fn recv(&mut self) -> Option<ViewerEvent> {
        self.event_rx.recv().await
    }
}

#[cfg(feature = "window")]
impl Drop for ViewerHandle {
    fn drop(&mut self) {
        // 句柄没了就没人收事件了，顺手把窗口关掉，避免 UI 线程泄漏。
        self.present.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewer_config_default_and_new() {
        let default = ViewerConfig::default();
        assert!(!default.fullscreen);
        assert!(!default.show_stats);
        assert_eq!(default.scale, None);
        assert_eq!(default.initial_size, (1280, 720));
        assert!(!default.title.is_empty());

        let titled = ViewerConfig::new("demo");
        assert_eq!(titled.title, "demo");
        assert_eq!(
            titled,
            ViewerConfig {
                title: "demo".to_string(),
                ..ViewerConfig::default()
            }
        );
    }

    #[test]
    fn viewer_config_builder_overrides_each_field() {
        let config = ViewerConfig::new("测试")
            .with_initial_size(1920, 1080)
            .with_fullscreen(true)
            .with_stats(true)
            .with_scale(Some(2.0));
        assert_eq!(config.title, "测试");
        assert_eq!(config.initial_size, (1920, 1080));
        assert!(config.fullscreen);
        assert!(config.show_stats);
        assert_eq!(config.scale, Some(2.0));
    }

    #[test]
    fn effective_initial_size_handles_zero_and_clamps() {
        let zero = ViewerConfig::default().with_initial_size(0, 0);
        assert_eq!(zero.effective_initial_size(), (1280, 720));

        let tiny = ViewerConfig::default().with_initial_size(1, 2);
        assert_eq!(tiny.effective_initial_size(), (16, 16));

        let huge = ViewerConfig::default().with_initial_size(100_000, 400);
        assert_eq!(huge.effective_initial_size(), (16_384, 400));

        // 单边为 0 只替换该边的默认值。
        let half = ViewerConfig::default().with_initial_size(0, 1080);
        assert_eq!(half.effective_initial_size(), (1280, 1080));
    }

    #[test]
    fn effective_scale_rejects_non_finite_and_clamps() {
        assert_eq!(ViewerConfig::default().effective_scale(), None);
        assert_eq!(
            ViewerConfig::default()
                .with_scale(Some(1.5))
                .effective_scale(),
            Some(1.5)
        );
        assert_eq!(
            ViewerConfig::default()
                .with_scale(Some(0.1))
                .effective_scale(),
            Some(0.25)
        );
        assert_eq!(
            ViewerConfig::default()
                .with_scale(Some(10.0))
                .effective_scale(),
            Some(4.0)
        );
        assert_eq!(
            ViewerConfig::default()
                .with_scale(Some(f64::NAN))
                .effective_scale(),
            None
        );
        assert_eq!(
            ViewerConfig::default()
                .with_scale(Some(f64::INFINITY))
                .effective_scale(),
            None
        );
    }

    #[test]
    fn viewer_control_is_comparable() {
        assert_ne!(ViewerControl::Continue, ViewerControl::Close);
        assert_eq!(ViewerControl::Continue, ViewerControl::Continue);
    }

    #[test]
    fn viewer_event_values_are_comparable() {
        // 公开事件类型要能拿来断言（避免以后加字段时破坏语义）。
        assert_eq!(
            ViewerEvent::Key {
                usage: 0x04,
                down: true,
                modifiers: 0
            },
            ViewerEvent::Key {
                usage: 0x04,
                down: true,
                modifiers: 0
            }
        );
        assert_ne!(
            ViewerEvent::Wheel { dx: 0, dy: 120 },
            ViewerEvent::Wheel { dx: 0, dy: -120 }
        );
        assert_eq!(
            ViewerEvent::Mouse {
                x: 1,
                y: 2,
                width: 3,
                height: 4,
                button: Some(1),
                down: true
            },
            ViewerEvent::Mouse {
                x: 1,
                y: 2,
                width: 3,
                height: 4,
                button: Some(1),
                down: true
            }
        );
    }
}
