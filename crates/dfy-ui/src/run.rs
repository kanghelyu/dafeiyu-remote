// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 阻塞式入口：在当前线程跑 winit 事件循环，直到窗口关闭或帧来源结束。
//!
//! 这是 CLI `view` 子命令要用的形态：
//!
//! ```text
//! 解码任务 ──(tokio mpsc)──> FrameSource（channel 已实现该 trait）──> run_viewer(config, source)
//! ```
//!
//! 帧来源在独立线程被轮询（[`crate::FrameSource`]），推帧只写「最新帧邮箱」并唤醒事件循环；
//! 事件循环退出后该线程会在一个轮询间隔内收到停止标志并结束。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use crate::app::{build_event_loop, EventSink, ViewerApp};
use crate::mailbox::{FrameMailbox, UiCommand};
use crate::source::pump_frames;
use crate::{FrameSource, ViewerConfig, ViewerControl, ViewerEvent};

/// 在当前线程显示 viewer，直到窗口关闭/用户按 `Esc`/帧来源结束。
///
/// 只接收事件不处理时用这个；需要在主线程翻译输入（转发到远端）时用 [`run_viewer_with`]。
///
/// # 线程要求
///
/// - Windows：可以从任意线程调用（事件循环用 `with_any_thread` 创建），因此可以放进
///   tokio 的 `spawn_blocking`；
/// - macOS：必须在主线程调用；
/// - Linux/X11/Wayland：winit 0.30 允许任意线程，但 Wayland 的某些合成器仍要求主线程，
///   稳妥做法是 CLI 在主线程调用。
///
/// # Errors
///
/// 事件循环创建失败、窗口创建失败、或帧来源线程无法启动时返回错误。
pub fn run_viewer(config: ViewerConfig, frames: impl FrameSource) -> anyhow::Result<()> {
    run_viewer_with(config, frames, |_event| ViewerControl::Continue)
}

/// 同 [`run_viewer`]，但每个窗口事件都会同步回调 `on_event`，回调返回
/// [`ViewerControl::Close`] 可请求关闭窗口（在 UI 线程上执行，**不要阻塞**）。
///
/// 典型用法：把 [`ViewerEvent::Key`]/[`ViewerEvent::Mouse`]/[`ViewerEvent::Wheel`]
/// 翻译成 `dfy-input::InputEvent` 交给会话的输入通道；收到 [`ViewerEvent::ToggleControl`]
/// 切换「是否允许远控输入」。
///
/// `on_event` 不必是 `'static`（闭包可以借用调用方的局部状态，因为本函数在窗口关闭后才返回）。
///
/// # Errors
///
/// 同 [`run_viewer`]。
pub fn run_viewer_with<E>(
    config: ViewerConfig,
    frames: impl FrameSource,
    on_event: E,
) -> anyhow::Result<()>
where
    E: FnMut(ViewerEvent) -> ViewerControl,
{
    let event_loop = build_event_loop()?;
    let proxy = event_loop.create_proxy();
    let mailbox = Arc::new(FrameMailbox::new());
    let stop = Arc::new(AtomicBool::new(false));

    let mut app = ViewerApp::new(
        config,
        EventSink::Callback(Box::new(on_event)),
        mailbox.clone(),
    );

    // 帧来源线程：轮询 source，把帧写进邮箱并通过 proxy 唤醒事件循环。
    let pump_mailbox = mailbox.clone();
    let pump_stop = stop.clone();
    let wake = proxy.clone();
    let ended = proxy.clone();
    let mut frames = frames;
    let pump = thread::Builder::new()
        .name("dfy-viewer-frames".to_string())
        .spawn(move || {
            let mut deliver = |frame: crate::Frame| -> bool {
                pump_mailbox.store_frame(frame);
                wake.send_event(UiCommand::Frame).is_ok()
            };
            // 帧来源结束 = 远端流结束：通知 UI 投递 Closed 并退出窗口。
            let mut on_closed = || -> bool { ended.send_event(UiCommand::FramesEnded).is_ok() };
            pump_frames(&mut frames, &pump_stop, &mut deliver, &mut on_closed)
        })?;

    let run_result = event_loop.run_app(&mut app);

    // 通知推帧线程退出并回收（它最多再睡一个 poll_interval）。
    stop.store(true, Ordering::Release);
    if pump.join().is_err() {
        tracing::warn!("viewer 推帧线程异常退出（panic）");
    }

    if let Some(message) = app.init_error() {
        anyhow::bail!("{message}");
    }
    run_result.map_err(anyhow::Error::from)
}
