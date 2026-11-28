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

use std::num::NonZeroU32;
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Fullscreen, Window, WindowAttributes, WindowId};

use crate::frame::{self, Frame};
use crate::keymap;
use crate::mailbox::{FrameMailbox, UiCommand};
use crate::scroll::{self, WheelAccumulator};
use crate::stats::RenderStats;
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
}

impl<'a> ViewerApp<'a> {
    pub(crate) fn new(
        config: ViewerConfig,
        sink: EventSink<'a>,
        mailbox: Arc<FrameMailbox>,
    ) -> Self {
        Self {
            surface: None,
            context: None,
            window: None,
            fullscreen: config.fullscreen,
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

    fn size_window_to_frame(&mut self, frame_w: u32, frame_h: u32) {
        let Some(window) = self.window.clone() else {
            return;
        };
        let scale = self.config.effective_scale().unwrap_or(1.0);
        let width = scaled_dimension(frame_w, scale);
        let height = scaled_dimension(frame_h, scale);
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
        if let Some(frame) = self.mailbox.take_latest() {
            self.last_frame = Some(frame);
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
                            dst_w,
                            dst_h,
                        );
                    }
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
            if let Some(size) = self.frame_size {
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
        let down = *state == ElementState::Pressed;

        // 修饰键位图先更新，保证同一次 Key 事件里的 modifiers 是「当前」状态。
        if let Some(bit) = keymap::modifier_bit(*code) {
            if down {
                self.modifiers |= 1u8 << bit;
            } else {
                self.modifiers &= !(1u8 << bit);
            }
        }

        // 原始按键（含修饰键本身）先转发；快捷键不吞原始事件。
        if let Some(usage) = keymap::hid_usage(*code) {
            self.emit(ViewerEvent::Key {
                usage,
                down,
                modifiers: self.modifiers,
            });
            if self.exit_requested {
                return;
            }
        }

        // 非重复的「按下」才触发本地快捷键。
        if !down || *repeat {
            return;
        }
        match code {
            KeyCode::F11 => self.toggle_fullscreen(),
            KeyCode::Escape => self.send_closed(),
            KeyCode::KeyQ if self.control_toggle_pressed() => {
                self.emit(ViewerEvent::ToggleControl);
            }
            _ => {}
        }
    }

    fn control_toggle_pressed(&self) -> bool {
        modifiers_have_control_toggle(self.modifiers)
    }

    fn toggle_fullscreen(&mut self) {
        let Some(window) = self.window.clone() else {
            return;
        };
        self.fullscreen = !self.fullscreen;
        if self.fullscreen {
            window.set_fullscreen(Some(Fullscreen::Borderless(None)));
        } else {
            window.set_fullscreen(None);
        }
        self.emit(ViewerEvent::ToggleFullscreen);
        self.request_redraw();
    }

    fn on_cursor_moved(&mut self, x: f64, y: f64) {
        let Some(window) = self.window.clone() else {
            return;
        };
        let Some((src_w, src_h)) = self.frame_size else {
            return;
        };
        let size = window.inner_size();
        let Some((frame_x, frame_y)) =
            frame::cursor_to_frame(x, y, size.width, size.height, src_w, src_h)
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
        let Some((src_w, src_h)) = self.frame_size else {
            return;
        };
        let (frame_x, frame_y) = self.last_cursor_frame.unwrap_or((0, 0));
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
        let (dx, dy) = scroll::scroll_to_wheel(delta, &mut self.wheel_accum);
        // 像素级滚动可能不足一格：累加器继续攒，不产生 0 增量事件。
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
            .with_inner_size(PhysicalSize::new(width, height));
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
            UiCommand::Frame => self.request_redraw(),
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
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                // 惰性 resize：把记录的尺寸标脏，下一次 render 里重新 resize surface。
                // 若在这里直接记成新尺寸，render 会跳过 surface.resize，
                // softbuffer 仍按旧尺寸取缓冲（或 surface 未设置尺寸时 panic）。
                self.surface_size = (0, 0);
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
            WindowEvent::MouseInput { state, button, .. } => {
                self.on_mouse_input(state, button);
            }
            WindowEvent::MouseWheel { delta, .. } => self.on_wheel(delta),
            WindowEvent::Focused(false) => {
                // 失焦时窗口收不到抬起事件：清掉修饰键，避免远端「卡 Ctrl」。
                self.modifiers = 0;
            }
            _ => {}
        }
        if self.exit_requested {
            event_loop.exit();
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
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
