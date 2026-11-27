// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 帧来源抽象：把「解码/网络侧产生帧」与「窗口事件循环」解耦。
//!
//! 窗口事件循环必须由 winit 在特定线程上驱动，而帧通常来自 tokio 任务或解码线程。
//! 本模块定义了可替换的 [`FrameSource`]：
//!
//! - [`crate::run_viewer`] 接收任意 `FrameSource`，内部起一个「帧泵」线程按
//!   [`FrameSource::poll_interval`] 轮询它，把帧推进单槽邮箱；
//! - CLI 的 `view` 子命令可以把 `tokio::sync::mpsc::Receiver<Frame>`（或 std channel）
//!   直接传进来——本模块已经为这两种接收端实现了 `FrameSource`；
//! - 测试可以用一个「按脚本吐帧」的假来源，完全不碰窗口/网络。
//!
//! # 契约
//!
//! [`FrameSource::try_next_frame`] **必须非阻塞**：没帧就返回 [`FramePoll::Pending`]，
//! 来源结束（不会再有帧，例如解码线程已退出）返回 [`FramePoll::Closed`]。
//! `run_viewer` 看到 `Closed` 会自动关窗退出；返回 `Pending` 时会睡
//! [`FrameSource::poll_interval`] 再试。

use std::time::{Duration, Instant};

#[cfg(any(feature = "window", test))]
use std::sync::atomic::{AtomicBool, Ordering};

use crate::frame::Frame;

/// 默认轮询间隔（8ms ≈ 125Hz），比常见的 60fps 解码略快，兼顾延迟与空转开销。
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(8);

/// 一次轮询的结果。
#[derive(Debug, Clone, PartialEq)]
pub enum FramePoll {
    /// 取到一帧（已校验的紧凑 BGRA）。
    Frame(Frame),
    /// 暂时没有新帧，稍后再轮询。
    Pending,
    /// 来源已结束：不会再有新帧，viewer 应自动关窗退出。
    Closed,
}

/// 可替换的帧来源（`run_viewer(config, frames)` 的 `frames` 参数）。
///
/// 实现必须 `Send + 'static`，因为帧泵跑在独立 OS 线程里。
/// 本 crate 已为下列接收端实现：
///
/// - [`tokio::sync::mpsc::UnboundedReceiver<Frame>`]
/// - [`tokio::sync::mpsc::Receiver<Frame>`]
/// - [`std::sync::mpsc::Receiver<Frame>`]
///
/// 也可以自己实现（例如直接包一层 H.264 解码器）。
pub trait FrameSource: Send + 'static {
    /// 非阻塞取下一帧。
    fn try_next_frame(&mut self) -> FramePoll;

    /// [`FramePoll::Pending`] 之后帧泵的睡眠间隔。
    ///
    /// 默认 [`DEFAULT_POLL_INTERVAL`]；自带阻塞/节流的来源可以调大（例如离线回放）。
    fn poll_interval(&self) -> Duration {
        DEFAULT_POLL_INTERVAL
    }
}

impl FrameSource for tokio::sync::mpsc::UnboundedReceiver<Frame> {
    fn try_next_frame(&mut self) -> FramePoll {
        match self.try_recv() {
            Ok(frame) => FramePoll::Frame(frame),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => FramePoll::Pending,
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => FramePoll::Closed,
        }
    }
}

impl FrameSource for tokio::sync::mpsc::Receiver<Frame> {
    fn try_next_frame(&mut self) -> FramePoll {
        match self.try_recv() {
            Ok(frame) => FramePoll::Frame(frame),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => FramePoll::Pending,
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => FramePoll::Closed,
        }
    }
}

impl FrameSource for std::sync::mpsc::Receiver<Frame> {
    fn try_next_frame(&mut self) -> FramePoll {
        match self.try_recv() {
            Ok(frame) => FramePoll::Frame(frame),
            Err(std::sync::mpsc::TryRecvError::Empty) => FramePoll::Pending,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => FramePoll::Closed,
        }
    }
}

/// 把一个 `Frame` 迭代器包装成 [`FrameSource`]，可按固定间隔节流。
///
/// 主要用于 demo/测试/离线回放：帧之间不需要真实解码节奏时，用一个惰性迭代器
///（例如 `std::iter::from_fn`）即可。迭代器耗尽后返回 [`FramePoll::Closed`]。
pub struct FrameIter<I: Iterator> {
    iter: std::iter::Peekable<I>,
    interval: Duration,
    next_allowed_at: Option<Instant>,
}

impl<I: Iterator> FrameIter<I> {
    /// 用迭代器构造（不限速：每次轮询都尽量吐一帧）。
    pub fn new(iter: I) -> Self {
        Self {
            iter: iter.peekable(),
            interval: Duration::ZERO,
            next_allowed_at: None,
        }
    }

    /// 设定帧之间的最小间隔（间隔内轮询返回 [`FramePoll::Pending`]）。
    #[must_use]
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }
}

impl<I: Iterator<Item = Frame> + Send + 'static> FrameSource for FrameIter<I> {
    fn try_next_frame(&mut self) -> FramePoll {
        // 先确认迭代器还有没有下一帧：耗尽时立即 Closed，不被节流闸门拖延退出。
        if self.iter.peek().is_none() {
            return FramePoll::Closed;
        }
        if let Some(next_allowed_at) = self.next_allowed_at {
            if Instant::now() < next_allowed_at {
                return FramePoll::Pending;
            }
        }
        match self.iter.next() {
            Some(frame) => {
                self.next_allowed_at = if self.interval.is_zero() {
                    None
                } else {
                    Some(Instant::now() + self.interval)
                };
                FramePoll::Frame(frame)
            }
            // `peek` 已确认有帧，正常不会走到这里；保守返回 Closed。
            None => FramePoll::Closed,
        }
    }
}

/// 帧泵的退出原因（便于日志区分「窗口关了」和「来源结束」）。
#[cfg(any(feature = "window", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PumpExit {
    /// 停止标志被置位（窗口已关闭）。
    Stopped,
    /// 帧来源返回 `Closed`，且成功通知了 UI 线程。
    SourceClosed,
    /// 投递失败：`EventLoopProxy` 已关闭（UI 线程已退出）。
    WakerClosed,
}

/// 帧泵：把 `source` 的帧推到 `deliver`，直到来源结束/停止标志/投递失败。
///
/// - `deliver` 返回 `false` 表示 UI 线程已经不在（`EventLoopProxy::send_event` 失败），
///   帧泵应立即退出，否则会一直空转。
/// - `on_closed` 在来源返回 `Closed` 时调用一次，返回值同样表示 UI 线程是否还在。
#[cfg(any(feature = "window", test))]
pub(crate) fn pump_frames(
    source: &mut dyn FrameSource,
    stop: &AtomicBool,
    deliver: &mut dyn FnMut(Frame) -> bool,
    on_closed: &mut dyn FnMut() -> bool,
) -> PumpExit {
    loop {
        if stop.load(Ordering::Acquire) {
            return PumpExit::Stopped;
        }
        match source.try_next_frame() {
            FramePoll::Frame(frame) => {
                if !deliver(frame) {
                    return PumpExit::WakerClosed;
                }
            }
            FramePoll::Pending => {
                let interval = source.poll_interval();
                if interval.is_zero() {
                    // 来源自己没给节奏：让出 CPU，避免忙等成 100% 单核。
                    std::thread::yield_now();
                } else {
                    std::thread::sleep(interval);
                }
            }
            FramePoll::Closed => {
                return if on_closed() {
                    PumpExit::SourceClosed
                } else {
                    PumpExit::WakerClosed
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// 按脚本依次返回预设结果，脚本耗尽后返回 `Closed`。
    struct ScriptSource {
        steps: VecDeque<FramePoll>,
        interval: Duration,
    }

    impl ScriptSource {
        fn new(steps: Vec<FramePoll>) -> Self {
            Self {
                steps: steps.into(),
                interval: Duration::ZERO,
            }
        }
    }

    impl FrameSource for ScriptSource {
        fn try_next_frame(&mut self) -> FramePoll {
            self.steps.pop_front().unwrap_or(FramePoll::Closed)
        }

        fn poll_interval(&self) -> Duration {
            self.interval
        }
    }

    fn frame(fill: u8) -> Frame {
        Frame::new(1, 1, vec![fill, 0, 0, 0xFF]).expect("测试帧必须合法")
    }

    #[test]
    fn pump_delivers_frames_until_source_closed() {
        let mut source = ScriptSource::new(vec![
            FramePoll::Frame(frame(1)),
            FramePoll::Frame(frame(2)),
            FramePoll::Closed,
        ]);
        let stop = AtomicBool::new(false);
        let mut delivered = Vec::new();
        let mut closed_calls = 0u32;
        let exit = pump_frames(
            &mut source,
            &stop,
            &mut |f| {
                delivered.push(f.bgra()[0]);
                true
            },
            &mut || {
                closed_calls += 1;
                true
            },
        );
        assert_eq!(exit, PumpExit::SourceClosed);
        assert_eq!(delivered, vec![1, 2]);
        assert_eq!(closed_calls, 1, "来源结束时只能通知一次");
    }

    #[test]
    fn pump_stops_immediately_when_flag_already_set() {
        let mut source = ScriptSource::new(vec![FramePoll::Frame(frame(1))]);
        let stop = AtomicBool::new(true);
        let mut delivered = 0u32;
        let exit = pump_frames(
            &mut source,
            &stop,
            &mut |_| {
                delivered += 1;
                true
            },
            &mut || true,
        );
        assert_eq!(exit, PumpExit::Stopped);
        assert_eq!(delivered, 0);
    }

    #[test]
    fn pump_stops_when_delivery_fails() {
        let mut source =
            ScriptSource::new(vec![FramePoll::Frame(frame(1)), FramePoll::Frame(frame(2))]);
        let stop = AtomicBool::new(false);
        let mut attempts = 0u32;
        let exit = pump_frames(
            &mut source,
            &stop,
            &mut |_| {
                attempts += 1;
                false
            },
            &mut || true,
        );
        assert_eq!(exit, PumpExit::WakerClosed);
        assert_eq!(attempts, 1, "投递失败后不得继续取帧");
    }

    #[test]
    fn pump_keeps_polling_through_pending() {
        let mut source = ScriptSource::new(vec![
            FramePoll::Pending,
            FramePoll::Frame(frame(7)),
            FramePoll::Pending,
            FramePoll::Closed,
        ]);
        let stop = AtomicBool::new(false);
        let mut delivered = Vec::new();
        let exit = pump_frames(
            &mut source,
            &stop,
            &mut |f| {
                delivered.push(f.bgra()[0]);
                true
            },
            &mut || true,
        );
        assert_eq!(exit, PumpExit::SourceClosed);
        assert_eq!(delivered, vec![7]);
    }

    #[test]
    fn pump_reports_waker_closed_when_close_notification_fails() {
        let mut source = ScriptSource::new(vec![FramePoll::Closed]);
        let stop = AtomicBool::new(false);
        let exit = pump_frames(&mut source, &stop, &mut |_| true, &mut || false);
        assert_eq!(exit, PumpExit::WakerClosed);
    }

    #[test]
    fn std_channel_receiver_is_a_frame_source() {
        let (tx, mut rx) = std::sync::mpsc::channel::<Frame>();
        assert_eq!(rx.try_next_frame(), FramePoll::Pending);
        tx.send(frame(3)).expect("发送测试帧");
        assert_eq!(rx.try_next_frame(), FramePoll::Frame(frame(3)));
        assert_eq!(rx.try_next_frame(), FramePoll::Pending);
        drop(tx);
        assert_eq!(rx.try_next_frame(), FramePoll::Closed);
    }

    #[test]
    fn tokio_unbounded_receiver_is_a_frame_source() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Frame>();
        assert_eq!(rx.try_next_frame(), FramePoll::Pending);
        tx.send(frame(9)).expect("发送测试帧");
        assert_eq!(rx.try_next_frame(), FramePoll::Frame(frame(9)));
        drop(tx);
        assert_eq!(rx.try_next_frame(), FramePoll::Closed);
    }

    #[test]
    fn tokio_bounded_receiver_is_a_frame_source() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Frame>(2);
        assert_eq!(rx.try_next_frame(), FramePoll::Pending);
        tx.try_send(frame(4)).expect("发送测试帧");
        assert_eq!(rx.try_next_frame(), FramePoll::Frame(frame(4)));
        drop(tx);
        assert_eq!(rx.try_next_frame(), FramePoll::Closed);
    }

    #[test]
    fn frame_iter_yields_all_frames_then_closed() {
        let mut source = FrameIter::new(vec![frame(1), frame(2)].into_iter());
        assert_eq!(source.try_next_frame(), FramePoll::Frame(frame(1)));
        assert_eq!(source.try_next_frame(), FramePoll::Frame(frame(2)));
        assert_eq!(source.try_next_frame(), FramePoll::Closed);
        assert_eq!(source.try_next_frame(), FramePoll::Closed);
    }

    #[test]
    fn frame_iter_with_interval_paces_frames() {
        let mut source = FrameIter::new(vec![frame(1), frame(2)].into_iter())
            .with_interval(Duration::from_millis(20));
        assert_eq!(source.try_next_frame(), FramePoll::Frame(frame(1)));
        assert_eq!(
            source.try_next_frame(),
            FramePoll::Pending,
            "间隔未到必须返回 Pending"
        );
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(source.try_next_frame(), FramePoll::Frame(frame(2)));
        assert_eq!(source.try_next_frame(), FramePoll::Closed);
    }

    #[test]
    fn frame_iter_poll_interval_defaults_to_8ms() {
        let source = FrameIter::new(Vec::<Frame>::new().into_iter());
        assert_eq!(source.poll_interval(), DEFAULT_POLL_INTERVAL);
        assert_eq!(DEFAULT_POLL_INTERVAL, Duration::from_millis(8));
    }
}
