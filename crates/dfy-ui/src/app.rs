// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! winit 0.30 应用：窗口生命周期、softbuffer 软件上屏、输入翻译、快捷键、渲染统计。
//!
//! 本模块是**唯一**和窗口系统打交道的地方（`#[cfg(feature = "window")]`），
//! 纯逻辑（帧缩放/键位/滚轮/统计/帧来源）全在无窗口模块里，可无头单测。
//!
//! 两个入口复用同一个 [`ViewerApp`]：
//!
//! - [`spawn_viewer`]：UI 跑在独立 OS 线程，事件走 channel（常驻主控端模式）；
//! - [`crate::run_viewer`]：UI 跑在调用线程，事件走回调（CLI `view` 子命令模式）。

use std::collections::BTreeSet;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalSize};
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorGrabMode, CursorIcon, Fullscreen, Window, WindowAttributes, WindowId};

use crate::frame::{self, Frame};
use crate::keymap;
use crate::mailbox::{FrameMailbox, UiCommand};
use crate::scroll::{self, WheelAccumulator};
use crate::stats::{ConnectionStats, RenderStats};
use crate::toolbar;
use crate::toolbar::{InputGate, ToolbarAction, ToolbarLayout, ToolbarState, MIN_CLIENT_SIZE};
use crate::{ViewerConfig, ViewerControl, ViewerEvent};

/// 左/右 Cha 位（bit0 + bit4）。
const CTRL_LR: u8 = 0b0001_0001;
/// 左/右 Shift 位（bit1 + bit5）。
const SHIFT_LR: u8 = 0b0010_0010;
/// 左/右 Alt 位（bit2 + bit6）。
const ALT_LR: u8 = 0b0100_0100;

/// 是否同时按下了 Ctrl、Alt、Shift（左右任一侧均可）。
///
/// 注意不能写成 `modifiers & (CTRL_LR | SHIFT_LR | ALT_LR) == 掩码`：
/// 那样会把「左右两侧同时按下」当成必要条件，左手单手组合永远不触发。
fn modifiers_have_control_toggle(modifiers: u8) -> bool {
    modifiers & CTRL_LR != 0 && modifiers & SHIFT_LR != 0 && modifiers & ALT_LR != 0
}

/// 窗口事件去向：常驻模式走 channel，阻塞模式走回调。
pub(crate) enum EventSink<'a> {
    /// 投递到 tokio unbounded mpsc（接收方关闭 → viewer 自动退出）。
    Channel(tokio::sync::mpsc::UnboundedSender<ViewerEvent>),
    /// 同步回调；返回 [`ViewerControl::Close`] 表示调用方要求关闭。
    ///
    /// 允许借用调用方状态（生命周期 `'a`），[`crate::run_viewer_with`] 保证在事件循环
    /// 退出前不结束回调的生命周期。
    Callback(Box<dyn FnMut(ViewerEvent) -> ViewerControl + 'a>),
}

/// 一次事件投递的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SinkOutcome {
    /// 事件已投递/回调已处理且要求继续。
    Delivered,
    /// 接收端已关闭（UI 线程应退出）。
    Closed,
    /// 回调要求关闭窗口。
    StopRequested,
}

/// 绘制浮层所需的只读快照；在借用可变 surface 前收集，避免借用整个 `ViewerApp`。
struct OverlaySnapshot<'a> {
    toolbar: ToolbarState,
    hovered_action: Option<ToolbarAction>,
    local_press: Option<(ToolbarAction, MouseButton)>,
    toolbar_focus_index: usize,
    fullscreen: bool,
    paused: bool,
    frame_size: Option<(u32, u32)>,
    stats: &'a RenderStats,
    scale_factor: f64,
}

/// winit 应用状态机。
///
/// 字段顺序即析构顺序：`surface`/`context` 必须先于 `window` 释放。
pub(crate) struct ViewerApp<'a> {
    surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
    context: Option<softbuffer::Context<Arc<Window>>>,
    window: Option<Arc<Window>>,
    config: ViewerConfig,
    sink: EventSink<'a>,
    mailbox: Arc<FrameMailbox>,
    surface_size: (u32, u32),
    last_frame: Option<Frame>,
    frame_size: Option<(u32, u32)>,
    last_cursor_frame: Option<(u32, u32)>,
    wheel_accum: WheelAccumulator,
    modifiers: u8,
    fullscreen: bool,
    sized_from_frame: bool,
    redraw_pending: bool,
    closed_sent: bool,
    exit_requested: bool,
    init_error: Option<String>,
    stats: RenderStats,
    toolbar: ToolbarState,
    input_gate: InputGate,
    stats_visible: bool,
    scale_factor: f64,
    pointer_position: Option<(f64, f64)>,
    hovered_action: Option<ToolbarAction>,
    local_press: Option<(ToolbarAction, MouseButton)>,
    remote_buttons: BTreeSet<u8>,
    remote_keys: BTreeSet<u16>,
    suppressed_keys: BTreeSet<KeyCode>,
    toolbar_focus_index: usize,
    cursor_icon: CursorIcon,
}

impl<'a> ViewerApp<'a> {
    pub(crate) fn new(
        config: ViewerConfig,
        sink: EventSink<'a>,
        mailbox: Arc<FrameMailbox>,
    ) -> Self {
        let fullscreen = config.fullscreen;
        Self {
            surface: None,
            context: None,
            window: None,
            fullscreen,
            config,
            sink,
            mailbox,
            surface_size: (0, 0),
            last_frame: None,
            frame_size: None,
            last_cursor_frame: None,
            wheel_accum: WheelAccumulator::default(),
            modifiers: 0,
            sized_from_frame: false,
            redraw_pending: false,
            closed_sent: false,
            exit_requested: false,
            init_error: None,
            stats: RenderStats::default(),
            toolbar: ToolbarState::new(fullscreen),
            input_gate: InputGate::default(),
            stats_visible: false,
            scale_factor: 1.0,
            pointer_position: None,
            hovered_action: None,
            local_press: None,
            remote_buttons: BTreeSet::new(),
            remote_keys: BTreeSet::new(),
            suppressed_keys: BTreeSet::new(),
            toolbar_focus_index: 0,
            cursor_icon: CursorIcon::Default,
        }
    }

    /// 窗口创建阶段的错误（仅在 `resumed` 里可能发生）；事件循环退出后由
    /// [`crate::run_viewer_with`] 回报给调用方。
    pub(crate) fn init_error(&self) -> Option<&str> {
        self.init_error.as_deref()
    }

    fn send(&mut self, event: ViewerEvent) -> SinkOutcome {
        match &mut self.sink {
            EventSink::Channel(sender) => {
                if sender.send(event).is_ok() {
                    SinkOutcome::Delivered
                } else {
                    SinkOutcome::Closed
                }
            }
            EventSink::Callback(callback) => match callback(event) {
                ViewerControl::Continue => SinkOutcome::Delivered,
                ViewerControl::Close => SinkOutcome::StopRequested,
            },
        }
    }

    fn emit(&mut self, event: ViewerEvent) {
        match self.send(event) {
            SinkOutcome::Delivered => {}
            SinkOutcome::Closed | SinkOutcome::StopRequested => self.exit_requested = true,
        }
    }

    fn send_closed(&mut self) {
        if self.closed_sent {
            return;
        }
        self.closed_sent = true;
        self.release_all_remote_input();
        self.emit(ViewerEvent::Closed);
    }

    fn request_redraw(&mut self) {
        if self.redraw_pending {
            return;
        }
        if let Some(window) = &self.window {
            window.request_redraw();
            self.redraw_pending = true;
        }
    }

    fn current_layout(&self) -> ToolbarLayout {
        let (width, height) = self.window.as_ref().map_or((0, 0), |window| {
            let size = window.inner_size();
            (size.width, size.height)
        });
        ToolbarLayout::new(
            width,
            height,
            self.scale_factor,
            self.input_gate.paused(),
            self.stats_visible,
            self.toolbar.visible(),
        )
    }

    fn refresh_overlay(&mut self, now: Instant) {
        let layout = self.current_layout();
        let Some((x, y)) = self.pointer_position else {
            let changed = self.input_gate.set_overlay(false);
            self.toolbar.set_hovered(false, now);
            self.hovered_action = None;
            self.update_cursor_icon(CursorIcon::Default);
            if changed {
                self.release_all_remote_input();
                self.request_redraw();
            }
            return;
        };
        let overlay = layout.overlay_at(x, y);
        let entered = self.input_gate.set_overlay(overlay);
        let action = layout.action_at(x, y);
        let hovered_changed = self.hovered_action != action;
        self.hovered_action = action;
        let was_visible = self.toolbar.visible();
        self.toolbar.set_hovered(overlay, now);
        if overlay && entered {
            self.release_all_remote_input();
            self.last_cursor_frame = None;
            self.release_cursor_capture();
        }
        self.update_cursor_icon(if action.is_some() {
            CursorIcon::Pointer
        } else {
            CursorIcon::Default
        });
        if entered || hovered_changed || was_visible != self.toolbar.visible() {
            self.request_redraw();
        }
    }

    fn update_cursor_icon(&mut self, icon: CursorIcon) {
        if self.cursor_icon == icon {
            return;
        }
        self.cursor_icon = icon;
        if let Some(window) = &self.window {
            window.set_cursor(icon);
        }
    }

    fn release_cursor_capture(&self) {
        if let Some(window) = &self.window {
            if let Err(err) = window.set_cursor_grab(CursorGrabMode::None) {
                tracing::debug!(%err, "释放 viewer 光标捕获失败");
            }
        }
    }

    fn enforce_minimum_client_size(&mut self) {
        let Some(window) = self.window.clone() else {
            return;
        };
        let scale = if self.scale_factor.is_finite() && self.scale_factor > 0.0 {
            self.scale_factor
        } else {
            1.0
        };
        window.set_min_inner_size(Some(LogicalSize::new(MIN_CLIENT_SIZE.0, MIN_CLIENT_SIZE.1)));
        let min = PhysicalSize::new(
            (MIN_CLIENT_SIZE.0 * scale).round().max(1.0) as u32,
            (MIN_CLIENT_SIZE.1 * scale).round().max(1.0) as u32,
        );
        let size = window.inner_size();
        if size.width < min.width || size.height < min.height {
            let _ = window.request_inner_size(min);
        }
    }

    fn release_remote_button(&mut self, button: u8) {
        if !self.remote_buttons.remove(&button) {
            return;
        }
        let Some((src_w, src_h)) = self.frame_size else {
            return;
        };
        let (frame_x, frame_y) = self.last_cursor_frame.unwrap_or((0, 0));
        self.emit(ViewerEvent::Mouse {
            x: frame_x,
            y: frame_y,
            width: src_w,
            height: src_h,
            button: Some(button),
            down: false,
        });
    }

    fn release_remote_buttons(&mut self) {
        let buttons: Vec<u8> = self.remote_buttons.iter().copied().collect();
        self.remote_buttons.clear();
        self.wheel_accum = WheelAccumulator::default();
        let Some((src_w, src_h)) = self.frame_size else {
            return;
        };
        let (frame_x, frame_y) = self.last_cursor_frame.unwrap_or((0, 0));
        for button in buttons {
            self.emit(ViewerEvent::Mouse {
                x: frame_x,
                y: frame_y,
                width: src_w,
                height: src_h,
                button: Some(button),
                down: false,
            });
            if self.exit_requested {
                return;
            }
        }
    }

    fn release_all_remote_input(&mut self) {
        self.release_remote_buttons();
        if self.exit_requested {
            return;
        }
        let keys: Vec<u16> = self.remote_keys.iter().copied().collect();
        self.remote_keys.clear();
        for usage in keys {
            self.emit(ViewerEvent::Key {
                usage,
                down: false,
                modifiers: 0,
            });
            if self.exit_requested {
                return;
            }
        }
    }

    fn remote_input_allowed(&self) -> bool {
        self.input_gate.allows_remote_input()
    }

    fn size_window_to_frame(&mut self, frame_w: u32, frame_h: u32) {
        let Some(window) = self.window.clone() else {
            return;
        };
        let scale = self.config.effective_scale().unwrap_or(1.0);
        let dpi = if self.scale_factor.is_finite() && self.scale_factor > 0.0 {
            self.scale_factor
        } else {
            1.0
        };
        let min_width = (MIN_CLIENT_SIZE.0 * dpi).round().max(1.0) as u32;
        let min_height = (MIN_CLIENT_SIZE.1 * dpi).round().max(1.0) as u32;
        let width = scaled_dimension(frame_w, scale)
            .max(min_width)
            .min(ViewerConfig::MAX_INITIAL_SIZE);
        let height = scaled_dimension(frame_h, scale)
            .max(min_height)
            .min(ViewerConfig::MAX_INITIAL_SIZE);
        if window
            .request_inner_size(PhysicalSize::new(width, height))
            .is_some()
        {
            tracing::debug!(width, height, "窗口尺寸请求未被立即接受");
        }
    }

    fn render(&mut self) {
        let Some(window) = self.window.clone() else {
            return;
        };

        // 1. 取最新帧（邮箱只留最新一帧；取走即空）。
        let mut received_frame = false;
        if let Some(frame) = self.mailbox.take_latest() {
            self.last_frame = Some(frame);
            received_frame = true;
        }

        // 2. 首帧定窗（仅当配置了 scale 且未全屏时）。
        if let Some((frame_w, frame_h)) = self.last_frame.as_ref().map(Frame::size) {
            if self.frame_size != Some((frame_w, frame_h)) {
                self.frame_size = Some((frame_w, frame_h));
                if !self.sized_from_frame {
                    self.sized_from_frame = true;
                    self.size_window_to_frame(frame_w, frame_h);
                }
            }
        }

        // 3. 实际客户区尺寸（可能是 0：最小化/驱动未就绪，跳过）。
        let inner = window.inner_size();
        let (dst_w, dst_h) = (inner.width, inner.height);
        if dst_w == 0 || dst_h == 0 {
            return;
        }

        let started = Instant::now();
        let mut presented = false;
        let layout = self.current_layout();
        let Some(content) = layout.content() else {
            return;
        };
        let overlay = OverlaySnapshot {
            toolbar: self.toolbar,
            hovered_action: self.hovered_action,
            local_press: self.local_press,
            toolbar_focus_index: self.toolbar_focus_index,
            fullscreen: self.fullscreen,
            paused: self.input_gate.paused(),
            frame_size: self.frame_size,
            stats: &self.stats,
            scale_factor: self.scale_factor,
        };
        if let Some(surface) = self.surface.as_mut() {
            let Some(target) = surface_target(dst_w, dst_h) else {
                return;
            };
            if surface_needs_resize(self.surface_size, target) {
                match surface.resize(target.0, target.1) {
                    Ok(()) => self.surface_size = (dst_w, dst_h),
                    Err(err) => {
                        tracing::warn!(%err, "调整 softbuffer surface 尺寸失败，跳过本帧");
                        return;
                    }
                }
            }

            match surface.buffer_mut() {
                Ok(mut buffer) => {
                    // 黑底（letterbox 区域和「无帧」状态都保持纯黑）。
                    buffer.fill(0);
                    if let Some(frame) = self.last_frame.as_ref() {
                        // `false` = 帧形状非法（理论上不会发生：Frame 已校验），黑屏处理。
                        let _ = frame::render_frame(
                            frame.bgra(),
                            frame.width(),
                            frame.height(),
                            &mut buffer[..],
                            content.width,
                            content.height,
                        );
                    }
                    let connection = self.mailbox.latest_stats();
                    Self::paint_overlays(
                        &mut buffer[..],
                        dst_w,
                        dst_h,
                        &layout,
                        connection.as_ref(),
                        &overlay,
                    );
                    window.pre_present_notify();
                    match buffer.present() {
                        Ok(()) => presented = true,
                        Err(err) => tracing::warn!(%err, "softbuffer 上屏失败"),
                    }
                }
                Err(err) => tracing::warn!(%err, "获取 softbuffer 缓冲失败"),
            }
        }

        // 4. 渲染统计 + 标题节流刷新（只在真正上屏后统计）。
        if presented {
            let now = Instant::now();
            let rendered_size = if received_frame {
                self.frame_size
            } else {
                None
            };
            if let Some(size) = rendered_size {
                self.stats.record_render(now, size, started.elapsed());
            }
            if self.config.show_stats && self.stats.should_refresh_title(now) {
                let conn = self.mailbox.latest_stats();
                window.set_title(&self.stats.title_with_connection(
                    &self.config.title,
                    true,
                    conn.as_ref(),
                ));
            }
        }
    }

    fn paint_overlays(
        buffer: &mut [u32],
        width: u32,
        height: u32,
        layout: &ToolbarLayout,
        connection: Option<&ConnectionStats>,
        overlay: &OverlaySnapshot<'_>,
    ) {
        if overlay.toolbar.visible() && layout.bar().width > 0 && layout.bar().height > 0 {
            fill_rect(buffer, width, height, layout.bar(), PANEL);
            stroke_rect(buffer, width, height, layout.bar(), BORDER);
            for (index, action) in ToolbarAction::ALL.into_iter().enumerate() {
                let Some(rect) = layout.control(action) else {
                    continue;
                };
                let hovered = overlay.hovered_action == Some(action);
                let pressed = overlay
                    .local_press
                    .is_some_and(|(pressed_action, _)| pressed_action == action);
                let focused = overlay.toolbar.focused() && overlay.toolbar_focus_index == index;
                if pressed || hovered || focused {
                    fill_rect(
                        buffer,
                        width,
                        height,
                        rect,
                        if pressed { PRESSED } else { HOVER },
                    );
                }
                if focused {
                    stroke_rect(buffer, width, height, rect, ACCENT);
                }
                draw_text_centered(
                    buffer,
                    width,
                    height,
                    rect,
                    layout.label(action, overlay.fullscreen, overlay.paused),
                    layout.font_scale(),
                    TEXT,
                );
            }
            if let Some(rect) = layout.status() {
                draw_text_centered(
                    buffer,
                    width,
                    height,
                    rect,
                    if overlay.paused {
                        "INPUT PAUSED"
                    } else {
                        "INPUT LIVE"
                    },
                    layout.font_scale(),
                    if overlay.paused { WARNING } else { SUCCESS },
                );
            }
            if let (Some(rect), Some((frame_w, frame_h))) = (layout.summary(), overlay.frame_size) {
                let path = connection.map_or("Direct", |stats| {
                    if stats.path.is_empty() {
                        "Direct"
                    } else {
                        stats.path.as_str()
                    }
                });
                let fps = overlay
                    .stats
                    .fps()
                    .map_or_else(|| "--".to_string(), |fps| format!("{fps:.0}"));
                let summary = format!("{frame_w}x{frame_h}  {fps} FPS  {path}");
                draw_text(
                    buffer,
                    width,
                    height,
                    rect,
                    &summary,
                    layout.font_scale(),
                    MUTED,
                );
            }
            if let Some(rect) = layout.stats() {
                Self::paint_stats(
                    buffer,
                    width,
                    height,
                    rect,
                    layout.stats_font_scale(),
                    connection,
                    overlay,
                );
            }
        }
        if let Some(rect) = layout.paused_banner() {
            fill_rect(buffer, width, height, rect, PAUSED_PANEL);
            stroke_rect(buffer, width, height, rect, WARNING);
            let scale = if overlay.scale_factor.is_finite() && overlay.scale_factor > 0.0 {
                overlay.scale_factor
            } else {
                1.0
            };
            let full_label_width = (200.0 * scale).round().max(1.0) as u32;
            let label = if rect.width >= full_label_width {
                "INPUT PAUSED | CTRL+ALT+SHIFT+Q"
            } else {
                "INPUT PAUSED"
            };
            let banner_font_scale = (scale.round().max(1.0)) as u32;
            draw_text_centered(
                buffer,
                width,
                height,
                rect,
                label,
                banner_font_scale,
                WARNING,
            );
        }
    }

    fn paint_stats(
        buffer: &mut [u32],
        width: u32,
        height: u32,
        rect: crate::frame::Rect,
        font_scale: u32,
        connection: Option<&ConnectionStats>,
        overlay: &OverlaySnapshot<'_>,
    ) {
        fill_rect(buffer, width, height, rect, PANEL);
        stroke_rect(buffer, width, height, rect, BORDER);
        let path = connection.map_or("Direct", |stats| {
            if stats.path.is_empty() {
                "Direct"
            } else {
                stats.path.as_str()
            }
        });
        let rtt = connection
            .and_then(|stats| stats.rtt_ms)
            .map_or_else(|| "--".to_string(), |value| format!("{value:.1} MS"));
        let loss = connection.and_then(|stats| stats.loss_ppm).map_or_else(
            || "--".to_string(),
            |value| format!("{:.2}%", f64::from(value) / 10_000.0),
        );
        let bitrate = connection
            .and_then(|stats| stats.bitrate_kbps)
            .map_or_else(|| "--".to_string(), |value| format!("{value} KBPS"));
        let decode = connection
            .and_then(|stats| stats.decode_ms)
            .map_or_else(|| "--".to_string(), |value| format!("{value:.1} MS"));
        let render = format!(
            "{:.1} MS",
            overlay.stats.last_render_time().as_secs_f64() * 1000.0
        );
        let frame = overlay
            .frame_size
            .map_or_else(|| "--".to_string(), |(w, h)| format!("{w}x{h}"));
        let fps = overlay
            .stats
            .fps()
            .map_or_else(|| "--".to_string(), |value| format!("{value:.1}"));
        let relay = connection
            .and_then(|stats| stats.relay_name.as_deref())
            .filter(|value| !value.is_empty())
            .unwrap_or("--");
        let lines = [
            "CONNECTION".to_string(),
            format!("PATH {path}"),
            format!("RTT {rtt}  LOSS {loss}"),
            format!("RATE {bitrate}"),
            format!("DECODE {decode}  RENDER {render}"),
            format!("FRAME {frame}  {fps} FPS"),
            format!("RELAY {relay}"),
        ];
        let line_height = font_scale.saturating_mul(9);
        let horizontal_padding = font_scale.saturating_mul(2);
        let horizontal_margin = font_scale.saturating_mul(4);
        for (index, line) in lines.iter().enumerate() {
            let y = rect
                .y
                .saturating_add((index as u32).saturating_mul(line_height));
            let line_rect = crate::frame::Rect {
                x: rect.x.saturating_add(horizontal_padding),
                y,
                width: rect.width.saturating_sub(horizontal_margin),
                height: line_height.min(rect.height.saturating_sub(y.saturating_sub(rect.y))),
            };
            draw_text(
                buffer,
                width,
                height,
                line_rect,
                line,
                font_scale,
                if index == 0 { ACCENT } else { TEXT },
            );
        }
    }

    fn on_keyboard(&mut self, event: &KeyEvent) {
        let KeyEvent {
            physical_key: PhysicalKey::Code(code),
            state,
            repeat,
            ..
        } = event
        else {
            return;
        };
        self.on_key(*code, *state, *repeat);
    }

    fn on_key(&mut self, code: KeyCode, state: ElementState, repeat: bool) {
        let down = state == ElementState::Pressed;
        if let Some(bit) = keymap::modifier_bit(code) {
            if down {
                self.modifiers |= 1u8 << bit;
            } else {
                self.modifiers &= !(1u8 << bit);
            }
        }
        if !down {
            if self.suppressed_keys.remove(&code) {
                return;
            }
            if self.remote_input_allowed() {
                if let Some(usage) = keymap::hid_usage(code) {
                    if self.remote_keys.remove(&usage) {
                        self.emit(ViewerEvent::Key {
                            usage,
                            down: false,
                            modifiers: self.modifiers,
                        });
                    }
                }
            }
            return;
        }

        let now = Instant::now();
        if !repeat {
            if code == KeyCode::F6 {
                let focused = !self.toolbar.focused();
                self.toolbar_focus_index = 0;
                self.toolbar.set_focused(focused, now);
                self.suppressed_keys.insert(code);
                self.request_redraw();
                return;
            }
            if code == KeyCode::KeyQ && self.control_toggle_pressed() {
                self.toggle_input(now);
                self.suppressed_keys.insert(code);
                return;
            }
            if code == KeyCode::F11 {
                self.toggle_fullscreen();
                self.suppressed_keys.insert(code);
                return;
            }
            if code == KeyCode::Escape && !self.toolbar.focused() {
                self.send_closed();
                self.suppressed_keys.insert(code);
                return;
            }
        }
        if self.toolbar.focused() {
            self.suppressed_keys.insert(code);
            if !repeat {
                match code {
                    KeyCode::ArrowRight | KeyCode::Tab => {
                        self.toolbar_focus_index = (self.toolbar_focus_index + 1) % 4;
                    }
                    KeyCode::ArrowLeft => {
                        self.toolbar_focus_index = (self.toolbar_focus_index + 3) % 4;
                    }
                    KeyCode::Enter | KeyCode::Space => {
                        self.activate_toolbar(ToolbarAction::ALL[self.toolbar_focus_index], now);
                    }
                    KeyCode::Escape | KeyCode::F6 => {
                        self.toolbar.set_focused(false, now);
                    }
                    _ => {}
                }
            }
            self.request_redraw();
            return;
        }

        if !self.remote_input_allowed() {
            self.suppressed_keys.insert(code);
            return;
        }
        if let Some(usage) = keymap::hid_usage(code) {
            if repeat && !self.remote_keys.contains(&usage) {
                self.suppressed_keys.insert(code);
                return;
            }
            if !repeat {
                self.remote_keys.insert(usage);
            }
            self.emit(ViewerEvent::Key {
                usage,
                down: true,
                modifiers: self.modifiers,
            });
        }
    }

    fn control_toggle_pressed(&self) -> bool {
        modifiers_have_control_toggle(self.modifiers)
    }

    fn toggle_input(&mut self, now: Instant) {
        if !self.input_gate.paused() {
            self.release_all_remote_input();
        }
        self.input_gate.toggle_paused();
        self.toolbar.reveal(now);
        self.emit(ViewerEvent::ToggleControl);
        self.refresh_overlay(now);
        self.request_redraw();
    }

    fn activate_toolbar(&mut self, action: ToolbarAction, now: Instant) {
        match action {
            ToolbarAction::Fullscreen => self.toggle_fullscreen(),
            ToolbarAction::ToggleInput => self.toggle_input(now),
            ToolbarAction::Stats => {
                self.stats_visible = !self.stats_visible;
                self.toolbar.reveal(now);
                self.refresh_overlay(now);
                self.request_redraw();
            }
            ToolbarAction::Disconnect => self.send_closed(),
        }
    }

    fn toggle_fullscreen(&mut self) {
        let Some(window) = self.window.clone() else {
            return;
        };
        self.fullscreen = !self.fullscreen;
        self.toolbar.set_fullscreen(self.fullscreen, Instant::now());
        self.toolbar_focus_index = 0;
        if self.fullscreen {
            window.set_fullscreen(Some(Fullscreen::Borderless(None)));
        } else {
            window.set_fullscreen(None);
        }
        self.emit(ViewerEvent::ToggleFullscreen);
        self.request_redraw();
    }

    fn on_cursor_moved(&mut self, x: f64, y: f64) {
        if self.window.is_none() {
            return;
        }
        self.pointer_position = Some((x, y));
        let now = Instant::now();
        let revealed = self.toolbar.reveal(now);
        self.refresh_overlay(now);
        if revealed {
            self.request_redraw();
        }
        if self.local_press.is_some() || !self.remote_input_allowed() {
            return;
        }
        let Some((src_w, src_h)) = self.frame_size else {
            return;
        };
        let layout = self.current_layout();
        let Some(content) = layout.content() else {
            return;
        };
        let Some((frame_x, frame_y)) =
            toolbar::map_cursor_to_remote(x, y, content, (src_w, src_h), &self.input_gate, &layout)
        else {
            return;
        };
        self.last_cursor_frame = Some((frame_x, frame_y));
        self.emit(ViewerEvent::Mouse {
            x: frame_x,
            y: frame_y,
            width: src_w,
            height: src_h,
            button: None,
            down: false,
        });
    }

    fn on_mouse_input(&mut self, state: ElementState, button: MouseButton) {
        let Some(hid_button) = keymap::hid_button(button) else {
            return;
        };
        let now = Instant::now();
        if let Some((action, pressed_button)) = self.local_press {
            if state == ElementState::Released && button == pressed_button {
                self.local_press = None;
                self.toolbar.set_pressed(None, now);
                let activate = self
                    .pointer_position
                    .and_then(|(x, y)| self.current_layout().action_at(x, y))
                    == Some(action);
                if activate {
                    self.activate_toolbar(action, now);
                }
                self.refresh_overlay(now);
                self.request_redraw();
                return;
            }
        }
        if self.local_press.is_some() {
            return;
        }

        let layout = self.current_layout();
        let overlay = self
            .pointer_position
            .is_some_and(|(x, y)| layout.overlay_at(x, y));
        if state == ElementState::Pressed && (self.input_gate.paused() || overlay) {
            self.release_remote_button(hid_button);
            if overlay && button == MouseButton::Left {
                if let Some(action) = self
                    .pointer_position
                    .and_then(|(x, y)| layout.action_at(x, y))
                {
                    self.local_press = Some((action, button));
                    self.toolbar.set_pressed(Some(action), now);
                    self.toolbar.reveal(now);
                    self.release_cursor_capture();
                    self.request_redraw();
                }
            }
            return;
        }
        if !self.remote_input_allowed() {
            if state == ElementState::Pressed {
                self.release_remote_button(hid_button);
            }
            return;
        }
        let Some((src_w, src_h)) = self.frame_size else {
            return;
        };
        let Some(content) = layout.content() else {
            return;
        };
        if state == ElementState::Pressed {
            let Some((x, y)) = self.pointer_position else {
                return;
            };
            let Some(mapped) = toolbar::map_cursor_to_remote(
                x,
                y,
                content,
                (src_w, src_h),
                &self.input_gate,
                &layout,
            ) else {
                return;
            };
            self.last_cursor_frame = Some(mapped);
            self.remote_buttons.insert(hid_button);
        } else if !self.remote_buttons.remove(&hid_button) {
            return;
        }
        let Some((frame_x, frame_y)) = self.last_cursor_frame else {
            return;
        };
        self.emit(ViewerEvent::Mouse {
            x: frame_x,
            y: frame_y,
            width: src_w,
            height: src_h,
            button: Some(hid_button),
            down: state == ElementState::Pressed,
        });
    }

    fn on_wheel(&mut self, delta: MouseScrollDelta) {
        let overlay = self
            .pointer_position
            .is_some_and(|(x, y)| self.current_layout().overlay_at(x, y));
        if self.local_press.is_some() || !self.remote_input_allowed() || overlay {
            self.wheel_accum = WheelAccumulator::default();
            return;
        }
        let (dx, dy) = scroll::scroll_to_wheel(delta, &mut self.wheel_accum);
        if dx != 0 || dy != 0 {
            self.emit(ViewerEvent::Wheel { dx, dy });
        }
    }

    fn sink_closed(&self) -> bool {
        match &self.sink {
            EventSink::Channel(sender) => sender.is_closed(),
            EventSink::Callback(_) => false,
        }
    }

    fn attach_window(&mut self, event_loop: &ActiveEventLoop) {
        let (width, height) = self.config.effective_initial_size();
        let mut attributes = WindowAttributes::default()
            .with_title(self.config.title.clone())
            .with_inner_size(PhysicalSize::new(width, height))
            .with_min_inner_size(LogicalSize::new(MIN_CLIENT_SIZE.0, MIN_CLIENT_SIZE.1));
        if let Some(icon) = load_app_icon() {
            attributes = attributes.with_window_icon(Some(icon));
        }
        let window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(err) => {
                tracing::error!(%err, "创建 viewer 窗口失败");
                self.init_error = Some(format!("创建 viewer 窗口失败: {err}"));
                event_loop.exit();
                return;
            }
        };

        let context = match softbuffer::Context::new(window.clone()) {
            Ok(context) => context,
            Err(err) => {
                tracing::error!(%err, "创建 softbuffer 上下文失败");
                self.init_error = Some(format!("创建 softbuffer 上下文失败: {err}"));
                event_loop.exit();
                return;
            }
        };
        let surface = match softbuffer::Surface::new(&context, window.clone()) {
            Ok(surface) => surface,
            Err(err) => {
                tracing::error!(%err, "创建 softbuffer surface 失败");
                self.init_error = Some(format!("创建 softbuffer surface 失败: {err}"));
                event_loop.exit();
                return;
            }
        };

        if self.fullscreen {
            window.set_fullscreen(Some(Fullscreen::Borderless(None)));
        }
        self.scale_factor = window.scale_factor();
        self.toolbar.set_fullscreen(self.fullscreen, Instant::now());
        self.enforce_minimum_client_size();
        // 全屏或「不按首帧定窗」时，后续帧尺寸变化不再动窗口。
        self.sized_from_frame = self.fullscreen || self.config.effective_scale().is_none();

        // surface 刚创建时还没有尺寸（softbuffer 要求先 resize 才能 buffer_mut）。
        // 不要用窗口尺寸预填充：那样 render 会以为尺寸已同步而跳过 surface.resize，
        // softbuffer 会 panic「Must set size of surface before calling buffer_mut()」。
        self.surface_size = (0, 0);
        let title = self.stats.title(&self.config.title, self.config.show_stats);
        window.set_title(&title);

        self.context = Some(context);
        self.surface = Some(surface);
        self.window = Some(window);
        self.mailbox.set_window_ready();
        self.request_redraw();
    }
}

impl ApplicationHandler<UiCommand> for ViewerApp<'_> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.sink_closed() || self.exit_requested {
            event_loop.exit();
            return;
        }
        if self.window.is_some() {
            // 重复 resumed（挂起/恢复）：窗口还在，重绘即可。
            self.request_redraw();
            return;
        }
        event_loop.set_control_flow(ControlFlow::Wait);
        self.attach_window(event_loop);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UiCommand) {
        match event {
            UiCommand::Frame | UiCommand::StatsUpdated => self.request_redraw(),
            UiCommand::FramesEnded | UiCommand::Close => {
                self.send_closed();
                event_loop.exit();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        if self
            .window
            .as_ref()
            .is_some_and(|window| window.id() != window_id)
        {
            return;
        }
        match event {
            WindowEvent::CloseRequested | WindowEvent::Destroyed => {
                self.send_closed();
                event_loop.exit();
                return;
            }
            WindowEvent::Resized(_) => {
                // 惰性 resize：把记录的尺寸标脏，下一次 render 里重新 resize surface。
                // 若在这里直接记成新尺寸，render 会跳过 surface.resize，
                // softbuffer 仍按旧尺寸取缓冲（或 surface 未设置尺寸时 panic）。
                self.surface_size = (0, 0);
                self.enforce_minimum_client_size();
                self.refresh_overlay(Instant::now());
                self.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.scale_factor = scale_factor;
                self.surface_size = (0, 0);
                self.enforce_minimum_client_size();
                self.refresh_overlay(Instant::now());
                self.request_redraw();
            }
            WindowEvent::RedrawRequested => {
                self.redraw_pending = false;
                self.render();
            }
            WindowEvent::KeyboardInput {
                event,
                is_synthetic,
                ..
            } => {
                if !is_synthetic {
                    self.on_keyboard(&event);
                }
            }
            WindowEvent::ModifiersChanged(_) => {}
            WindowEvent::CursorMoved { position, .. } => {
                self.on_cursor_moved(position.x, position.y);
            }
            WindowEvent::CursorEntered { .. } => self.refresh_overlay(Instant::now()),
            WindowEvent::CursorLeft { .. } => {
                self.pointer_position = None;
                self.release_remote_buttons();
                self.last_cursor_frame = None;
                self.local_press = None;
                self.toolbar.set_pressed(None, Instant::now());
                self.refresh_overlay(Instant::now());
            }
            WindowEvent::MouseInput { state, button, .. } => {
                self.on_mouse_input(state, button);
            }
            WindowEvent::MouseWheel { delta, .. } => self.on_wheel(delta),
            WindowEvent::Focused(false) => {
                self.release_all_remote_input();
                self.modifiers = 0;
                self.suppressed_keys.clear();
                self.pointer_position = None;
                self.local_press = None;
                self.toolbar.set_pressed(None, Instant::now());
                self.toolbar.set_focused(false, Instant::now());
                self.toolbar.set_hovered(false, Instant::now());
                self.update_cursor_icon(CursorIcon::Default);
                self.release_cursor_capture();
            }
            WindowEvent::Focused(true) => {
                self.toolbar.reveal(Instant::now());
                self.refresh_overlay(Instant::now());
                self.request_redraw();
            }
            _ => {}
        }
        if self.exit_requested {
            event_loop.exit();
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        if self.toolbar.update(now) {
            self.hovered_action = None;
            self.update_cursor_icon(CursorIcon::Default);
            self.request_redraw();
        }
        event_loop.set_control_flow(match self.toolbar.next_wakeup() {
            Some(deadline) => ControlFlow::WaitUntil(deadline),
            None => ControlFlow::Wait,
        });
        if self.sink_closed() {
            self.exit_requested = true;
        }
        if self.exit_requested {
            event_loop.exit();
        }
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        // 显式先于 window 释放 surface/context（字段顺序已保证，这里是双保险）。
        self.surface = None;
        self.context = None;
        self.window = None;
    }
}

const PANEL: u32 = 0x0018_1D24;
const HOVER: u32 = 0x002D_3742;
const PRESSED: u32 = 0x003C_4854;
const BORDER: u32 = 0x0044_5160;
const TEXT: u32 = 0x00E8_EDF2;
const MUTED: u32 = 0x00A9_B4C0;
const ACCENT: u32 = 0x004C_C9FF;
const SUCCESS: u32 = 0x006B_D9A0;
const WARNING: u32 = 0x00FF_B86B;
const PAUSED_PANEL: u32 = 0x0035_1416;

fn fill_rect(buffer: &mut [u32], width: u32, height: u32, rect: crate::frame::Rect, color: u32) {
    let x0 = rect.x.min(width);
    let y0 = rect.y.min(height);
    let x1 = rect.x.saturating_add(rect.width).min(width);
    let y1 = rect.y.saturating_add(rect.height).min(height);
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let row_width = (x1 - x0) as usize;
    for row in y0..y1 {
        let Some(start) = (row as usize).checked_mul(width as usize) else {
            continue;
        };
        let Some(end) = start.checked_add(row_width) else {
            continue;
        };
        if let Some(pixels) = buffer.get_mut(start..end) {
            pixels.fill(color);
        }
    }
}

fn fill_rect_clipped(
    buffer: &mut [u32],
    width: u32,
    height: u32,
    rect: crate::frame::Rect,
    clip: crate::frame::Rect,
    color: u32,
) {
    let x0 = rect.x.max(clip.x).min(width);
    let y0 = rect.y.max(clip.y).min(height);
    let x1 = rect
        .x
        .saturating_add(rect.width)
        .min(clip.x.saturating_add(clip.width))
        .min(width);
    let y1 = rect
        .y
        .saturating_add(rect.height)
        .min(clip.y.saturating_add(clip.height))
        .min(height);
    fill_rect(
        buffer,
        width,
        height,
        crate::frame::Rect {
            x: x0,
            y: y0,
            width: x1.saturating_sub(x0),
            height: y1.saturating_sub(y0),
        },
        color,
    );
}

fn stroke_rect(buffer: &mut [u32], width: u32, height: u32, rect: crate::frame::Rect, color: u32) {
    fill_rect(
        buffer,
        width,
        height,
        crate::frame::Rect {
            y: rect.y,
            height: 1,
            ..rect
        },
        color,
    );
    fill_rect(
        buffer,
        width,
        height,
        crate::frame::Rect {
            y: rect.y.saturating_add(rect.height.saturating_sub(1)),
            height: 1,
            ..rect
        },
        color,
    );
    fill_rect(
        buffer,
        width,
        height,
        crate::frame::Rect { width: 1, ..rect },
        color,
    );
    fill_rect(
        buffer,
        width,
        height,
        crate::frame::Rect {
            x: rect.x.saturating_add(rect.width.saturating_sub(1)),
            width: 1,
            ..rect
        },
        color,
    );
}

fn draw_text_centered(
    buffer: &mut [u32],
    width: u32,
    height: u32,
    rect: crate::frame::Rect,
    text: &str,
    font_scale: u32,
    color: u32,
) {
    let font_scale = font_scale.max(1);
    let text_width = text
        .chars()
        .count()
        .saturating_mul(6)
        .saturating_sub(1)
        .saturating_mul(font_scale as usize);
    let glyph_height = 7u32.saturating_mul(font_scale);
    let x = rect.x.saturating_add(
        rect.width
            .saturating_sub(text_width.min(u32::MAX as usize) as u32)
            / 2,
    );
    let y = rect
        .y
        .saturating_add(rect.height.saturating_sub(glyph_height) / 2);
    draw_text(
        buffer,
        width,
        height,
        crate::frame::Rect { x, y, ..rect },
        text,
        font_scale,
        color,
    );
}

fn draw_text(
    buffer: &mut [u32],
    width: u32,
    height: u32,
    rect: crate::frame::Rect,
    text: &str,
    font_scale: u32,
    color: u32,
) {
    let font_scale = font_scale.max(1);
    let mut cursor_x = rect.x;
    for character in text.chars() {
        let glyph = glyph(character.to_ascii_uppercase());
        for (row, bits) in glyph.iter().copied().enumerate() {
            for column in 0..5u32 {
                if bits & (1 << (4 - column)) == 0 {
                    continue;
                }
                fill_rect_clipped(
                    buffer,
                    width,
                    height,
                    crate::frame::Rect {
                        x: cursor_x.saturating_add(column.saturating_mul(font_scale)),
                        y: rect
                            .y
                            .saturating_add((row as u32).saturating_mul(font_scale)),
                        width: font_scale,
                        height: font_scale,
                    },
                    rect,
                    color,
                );
            }
        }
        cursor_x = cursor_x.saturating_add(6u32.saturating_mul(font_scale));
    }
}

fn glyph(character: char) -> [u8; 7] {
    match character {
        ' ' => [0; 7],
        'A' => [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'B' => [0x1E, 0x11, 0x11, 0x1E, 0x11, 0x11, 0x1E],
        'C' => [0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E],
        'D' => [0x1E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1E],
        'E' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F],
        'F' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10],
        'G' => [0x0E, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0E],
        'H' => [0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'I' => [0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E],
        'J' => [0x07, 0x02, 0x02, 0x02, 0x12, 0x12, 0x0C],
        'K' => [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11],
        'L' => [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F],
        'M' => [0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11],
        'N' => [0x11, 0x19, 0x15, 0x13, 0x11, 0x11, 0x11],
        'O' => [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'P' => [0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10],
        'Q' => [0x0E, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0D],
        'R' => [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11],
        'S' => [0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E],
        'T' => [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        'U' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'V' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x0A, 0x04],
        'W' => [0x11, 0x11, 0x11, 0x15, 0x15, 0x15, 0x0A],
        'X' => [0x11, 0x11, 0x0A, 0x04, 0x0A, 0x11, 0x11],
        'Y' => [0x11, 0x11, 0x0A, 0x04, 0x04, 0x04, 0x04],
        'Z' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1F],
        '0' => [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
        '1' => [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
        '2' => [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
        '3' => [0x1E, 0x01, 0x01, 0x0E, 0x01, 0x01, 0x1E],
        '4' => [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
        '5' => [0x1F, 0x10, 0x10, 0x1E, 0x01, 0x01, 0x1E],
        '6' => [0x0E, 0x10, 0x10, 0x1E, 0x11, 0x11, 0x0E],
        '7' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        '8' => [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
        '9' => [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x01, 0x0E],
        '-' => [0, 0, 0, 0x1F, 0, 0, 0],
        '+' => [0, 0x04, 0x04, 0x1F, 0x04, 0x04, 0],
        '.' => [0, 0, 0, 0, 0, 0x0C, 0x0C],
        ':' => [0, 0x0C, 0x0C, 0, 0x0C, 0x0C, 0],
        '/' => [0x01, 0x02, 0x02, 0x04, 0x08, 0x08, 0x10],
        '%' => [0x11, 0x12, 0x04, 0x08, 0x11, 0x06, 0x10],
        '(' => [0x02, 0x04, 0x08, 0x08, 0x08, 0x04, 0x02],
        ')' => [0x08, 0x04, 0x02, 0x02, 0x02, 0x04, 0x08],
        '|' => [0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        '_' => [0, 0, 0, 0, 0, 0, 0x1F],
        '>' => [0x10, 0x08, 0x04, 0x02, 0x04, 0x08, 0x10],
        '<' => [0x01, 0x02, 0x04, 0x08, 0x04, 0x02, 0x01],
        '=' => [0, 0x1F, 0, 0, 0x1F, 0, 0],
        _ => [0x0E, 0x11, 0x01, 0x02, 0x04, 0, 0x04],
    }
}

const ICON_PNG: &[u8] = include_bytes!("../../../assets/icon-512.png");

fn load_app_icon() -> Option<winit::window::Icon> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(ICON_PNG));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    let bytes = &buf[..info.buffer_size()];
    let rgba = match info.color_type {
        png::ColorType::Rgba => bytes.to_vec(),
        png::ColorType::Rgb => bytes
            .chunks_exact(3)
            .flat_map(|rgb| [rgb[0], rgb[1], rgb[2], 255])
            .collect(),
        _ => return None,
    };
    winit::window::Icon::from_rgba(rgba, info.width, info.height).ok()
}

/// 把「源像素 × scale」换算成夹在合法窗口范围里的物理像素。
fn scaled_dimension(source: u32, scale: f64) -> u32 {
    let low = f64::from(ViewerConfig::MIN_INITIAL_SIZE);
    let high = f64::from(ViewerConfig::MAX_INITIAL_SIZE);
    (f64::from(source) * scale).round().clamp(low, high) as u32
}

/// 把客户区尺寸换算成 softbuffer 要求的 `NonZeroU32` 尺寸。
///
/// 返回 `None` 表示宽或高为 0（窗口最小化/驱动还没给出尺寸）：softbuffer 的
/// `Surface::resize` 不接受 0，调用方必须跳过本帧。
fn surface_target(width: u32, height: u32) -> Option<(NonZeroU32, NonZeroU32)> {
    Some((NonZeroU32::new(width)?, NonZeroU32::new(height)?))
}

/// surface 是否必须重新 `resize`：只有**记录尺寸与当前目标完全一致**时才跳过。
///
/// 历史 bug：`attach_window` 里若用窗口尺寸预填 `surface_size`，首次 render 会误以为
/// 尺寸已同步而跳过 `resize`，随后 `buffer_mut()` 直接 panic（softbuffer 要求先设置
/// 尺寸）。现在 `surface_size` 初值恒为 `(0, 0)`，`WindowEvent::Resized` 也把它标脏，
/// 所以「新窗口 / 每次改窗口大小后」这里一定会走到 resize 分支。
fn surface_needs_resize(surface_size: (u32, u32), target: (NonZeroU32, NonZeroU32)) -> bool {
    surface_size != (target.0.get(), target.1.get())
}

/// 构造 winit 事件循环（`EventLoop<T>` 无法克隆，所以两个入口各自构造一个）。
///
/// Windows 上允许从任意线程创建/驱动（`with_any_thread(true)`），这样 CLI 可以在
/// tokio 的阻塞任务里调 [`crate::run_viewer`]；其他平台要求主线程，见模块文档。
pub(crate) fn build_event_loop() -> anyhow::Result<EventLoop<UiCommand>> {
    #[cfg(windows)]
    {
        use winit::platform::windows::EventLoopBuilderExtWindows;
        let mut builder = EventLoop::<UiCommand>::with_user_event();
        builder.with_any_thread(true);
        Ok(builder.build()?)
    }
    #[cfg(not(windows))]
    {
        Ok(EventLoop::<UiCommand>::with_user_event().build()?)
    }
}

/// 起一个独立线程跑 viewer 窗口，返回推帧/收事件句柄。
///
/// 线程模型：winit 事件循环跑在名为 `dfy-viewer-ui` 的 OS 线程；调用方通过
/// [`crate::PresenterHandle`] 跨线程推帧、通过 [`crate::ViewerHandle::event_rx`] 收事件。
///
/// 返回成功只表示事件循环已创建；窗口创建失败（极少见，如无显示服务）会记录错误日志并让
/// viewer 立即结束，`event_rx` 随后关闭。
///
/// # Errors
///
/// OS 线程创建失败、或事件循环初始化失败（如平台不允许从该线程创建）时返回错误。
pub fn spawn_viewer(config: ViewerConfig) -> anyhow::Result<crate::ViewerHandle> {
    let (proxy_tx, proxy_rx) =
        std::sync::mpsc::sync_channel::<Result<EventLoopProxy<UiCommand>, String>>(1);
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<ViewerEvent>();
    let mailbox = Arc::new(FrameMailbox::new());
    let thread_mailbox = mailbox.clone();

    thread::Builder::new()
        .name("dfy-viewer-ui".to_string())
        .spawn(move || {
            let event_loop = match build_event_loop() {
                Ok(event_loop) => event_loop,
                Err(err) => {
                    let _ = proxy_tx.send(Err(err.to_string()));
                    return;
                }
            };
            let proxy = event_loop.create_proxy();
            if proxy_tx.send(Ok(proxy.clone())).is_err() {
                // 调用方已经放弃（接收端 drop）：不必再跑事件循环。
                return;
            }
            let mut app = ViewerApp::new(config, EventSink::Channel(event_tx), thread_mailbox);
            if let Err(err) = event_loop.run_app(&mut app) {
                tracing::error!(%err, "viewer 事件循环异常退出");
            }
        })?;

    let proxy = match proxy_rx.recv() {
        Ok(Ok(proxy)) => proxy,
        Ok(Err(err)) => anyhow::bail!("创建 viewer 事件循环失败: {err}"),
        Err(_) => anyhow::bail!("viewer UI 线程在初始化完成前退出"),
    };

    Ok(crate::ViewerHandle {
        event_rx,
        present: crate::PresenterHandle { mailbox, proxy },
    })
}

/// 必须在主线程运行的 viewer 事件循环句柄（macOS AppKit 要求在主线程创建与驱动窗口）。
pub struct MainThreadViewer {
    event_loop: EventLoop<UiCommand>,
    app: ViewerApp<'static>,
}

impl MainThreadViewer {
    /// 在当前线程（macOS 必须为主线程）阻塞运行窗口事件循环直到关闭。
    pub fn run(mut self) -> anyhow::Result<()> {
        let run_result = self.event_loop.run_app(&mut self.app);
        if let Some(message) = self.app.init_error() {
            anyhow::bail!("{message}");
        }
        run_result.map_err(anyhow::Error::from)
    }
}

/// 创建主线程 viewer：返回可跨线程推帧/收事件的 [`crate::ViewerHandle`] 与必须在主线程执行的 [`MainThreadViewer`]。
pub fn create_main_thread_viewer(
    config: ViewerConfig,
) -> anyhow::Result<(crate::ViewerHandle, MainThreadViewer)> {
    let event_loop = build_event_loop()?;
    let proxy = event_loop.create_proxy();
    let mailbox = Arc::new(FrameMailbox::new());
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<ViewerEvent>();
    let app = ViewerApp::new(config, EventSink::Channel(event_tx), mailbox.clone());
    let handle = crate::ViewerHandle {
        event_rx,
        present: crate::PresenterHandle { mailbox, proxy },
    };
    Ok((handle, MainThreadViewer { event_loop, app }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn recording_app(events: &Rc<RefCell<Vec<ViewerEvent>>>) -> ViewerApp<'static> {
        let events = Rc::clone(events);
        ViewerApp::new(
            ViewerConfig::default(),
            EventSink::Callback(Box::new(move |event| {
                events.borrow_mut().push(event);
                ViewerControl::Continue
            })),
            Arc::new(FrameMailbox::new()),
        )
    }

    #[test]
    fn paused_input_is_suppressed_and_shortcut_resumes_with_held_modifiers() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut app = recording_app(&events);
        app.modifiers = (1u8 << 0) | (1u8 << 1) | (1u8 << 2);
        app.on_key(KeyCode::KeyQ, ElementState::Pressed, false);
        assert!(app.input_gate.paused());
        assert_eq!(&*events.borrow(), &[ViewerEvent::ToggleControl]);

        events.borrow_mut().clear();
        app.on_key(KeyCode::KeyA, ElementState::Pressed, false);
        app.on_key(KeyCode::KeyA, ElementState::Pressed, true);
        app.on_key(KeyCode::KeyA, ElementState::Released, false);
        app.on_mouse_input(ElementState::Pressed, MouseButton::Left);
        app.on_mouse_input(ElementState::Released, MouseButton::Left);
        app.on_wheel(MouseScrollDelta::LineDelta(0.0, 1.0));
        assert!(events.borrow().is_empty());

        app.on_key(KeyCode::KeyQ, ElementState::Pressed, false);
        assert!(!app.input_gate.paused());
        assert_eq!(&*events.borrow(), &[ViewerEvent::ToggleControl]);
        events.borrow_mut().clear();
        app.on_key(KeyCode::KeyA, ElementState::Pressed, false);
        assert_eq!(
            &*events.borrow(),
            &[ViewerEvent::Key {
                usage: 0x04,
                down: true,
                modifiers: app.modifiers,
            }]
        );
    }

    #[test]
    fn pausing_releases_held_remote_input_before_toggle_event() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut app = recording_app(&events);
        app.frame_size = Some((1280, 720));
        app.last_cursor_frame = Some((10, 20));
        app.on_key(KeyCode::KeyA, ElementState::Pressed, false);
        app.remote_buttons.insert(1);
        events.borrow_mut().clear();

        app.toggle_input(Instant::now());
        assert!(app.input_gate.paused());
        assert!(app.remote_keys.is_empty());
        assert!(app.remote_buttons.is_empty());
        assert_eq!(
            &*events.borrow(),
            &[
                ViewerEvent::Mouse {
                    x: 10,
                    y: 20,
                    width: 1280,
                    height: 720,
                    button: Some(1),
                    down: false,
                },
                ViewerEvent::Key {
                    usage: 0x04,
                    down: false,
                    modifiers: 0,
                },
                ViewerEvent::ToggleControl,
            ]
        );
    }

    #[test]
    fn focused_keyboard_and_local_press_never_forward_repeats() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut app = recording_app(&events);
        let now = Instant::now();
        app.toolbar.set_focused(true, now);
        app.on_key(KeyCode::ArrowRight, ElementState::Pressed, false);
        app.on_key(KeyCode::ArrowRight, ElementState::Pressed, true);
        app.on_key(KeyCode::KeyA, ElementState::Pressed, false);
        app.on_key(KeyCode::KeyA, ElementState::Pressed, true);
        app.on_key(KeyCode::KeyA, ElementState::Released, false);
        app.frame_size = Some((1280, 720));
        app.last_cursor_frame = Some((640, 360));
        app.pointer_position = Some((320.0, 200.0));
        app.local_press = Some((ToolbarAction::Fullscreen, MouseButton::Left));
        app.on_mouse_input(ElementState::Pressed, MouseButton::Right);
        app.on_wheel(MouseScrollDelta::LineDelta(0.0, 1.0));
        app.on_mouse_input(ElementState::Released, MouseButton::Left);
        assert!(app.local_press.is_none());
        assert!(events.borrow().is_empty());
    }

    #[test]
    fn toolbar_actions_use_existing_event_paths_and_close_once() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut app = recording_app(&events);
        let now = Instant::now();
        app.activate_toolbar(ToolbarAction::Stats, now);
        assert!(app.stats_visible);
        app.activate_toolbar(ToolbarAction::Stats, now);
        assert!(!app.stats_visible);
        app.activate_toolbar(ToolbarAction::ToggleInput, now);
        assert_eq!(&*events.borrow(), &[ViewerEvent::ToggleControl]);
        app.activate_toolbar(ToolbarAction::Disconnect, now);
        app.send_closed();
        assert_eq!(
            &*events.borrow(),
            &[ViewerEvent::ToggleControl, ViewerEvent::Closed]
        );
    }

    #[test]
    fn scaled_dimension_clamps_to_window_limits() {
        assert_eq!(scaled_dimension(1280, 1.0), 1280);
        assert_eq!(scaled_dimension(1280, 0.5), 640);
        // 下限：再小也至少 16 像素，避免 request_inner_size 传 0。
        assert_eq!(scaled_dimension(10, 0.1), ViewerConfig::MIN_INITIAL_SIZE);
        // 上限：超大首帧不能撑爆窗口。
        assert_eq!(
            scaled_dimension(20_000, 4.0),
            ViewerConfig::MAX_INITIAL_SIZE
        );
    }

    #[test]
    fn surface_target_rejects_zero_sides() {
        assert_eq!(surface_target(0, 720), None);
        assert_eq!(surface_target(1280, 0), None);
        assert_eq!(surface_target(0, 0), None);
        let target = surface_target(1920, 1080).expect("非零客户区必须有目标尺寸");
        assert_eq!((target.0.get(), target.1.get()), (1920, 1080));
    }

    #[test]
    fn surface_is_resized_on_first_frame_and_after_every_window_resize() {
        let initial = surface_target(1280, 720).expect("非零尺寸");
        // 刚 attach_window：surface_size 恒为 (0, 0)，首帧必须 resize（否则 buffer_mut panic）。
        assert!(surface_needs_resize((0, 0), initial));
        // resize 成功后记录尺寸：同样大小的帧不再重复 resize。
        assert!(!surface_needs_resize((1280, 720), initial));

        // Windows 的 Resized 事件会把 surface_size 标脏成 (0, 0)，必须按新尺寸再 resize。
        let resized = surface_target(1600, 900).expect("非零尺寸");
        assert!(surface_needs_resize((0, 0), resized));
        assert!(surface_needs_resize((1280, 720), resized));

        // DPI 不匹配（例如 200% 缩放改窗口后物理像素变了）同样属于「不一致」。
        let hidpi = surface_target(2560, 1440).expect("非零尺寸");
        assert!(surface_needs_resize((1280, 720), hidpi));
    }

    #[test]
    fn control_toggle_modifier_mask_covers_both_sides() {
        // 左 Ctrl + 左 Alt + 左 Shift：单手组合必须触发。
        assert!(modifiers_have_control_toggle(
            (1u8 << 0) | (1u8 << 1) | (1u8 << 2)
        ));
        // 右 Ctrl + 右 Alt + 右 Shift：同样算。
        assert!(modifiers_have_control_toggle(
            (1u8 << 4) | (1u8 << 5) | (1u8 << 6)
        ));
        // 左右混按也算（例如左 Ctrl + 右 Shift + 右 Alt）。
        assert!(modifiers_have_control_toggle(
            (1u8 << 0) | (1u8 << 5) | (1u8 << 6)
        ));
        // 缺 Shift 就不算。
        assert!(!modifiers_have_control_toggle((1u8 << 0) | (1u8 << 2)));
        // 只有修饰键组、没有第三组（这里缺 Alt）。
        assert!(!modifiers_have_control_toggle((1u8 << 0) | (1u8 << 1)));
        // 一个都没有。
        assert!(!modifiers_have_control_toggle(0));
    }

    #[test]
    fn app_icon_decodes_cleanly() {
        assert!(load_app_icon().is_some());
    }
}
