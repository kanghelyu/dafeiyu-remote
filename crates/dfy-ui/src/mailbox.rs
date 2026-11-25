// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 单槽「最新帧」邮箱 + UI 线程唤醒命令。
//!
//! 推帧线程只写 `latest`（覆盖式，不排队），UI 线程渲染前取走最新一帧；这样解码/网络线程
//! 永远不阻塞在 UI 上，帧率跟不上时丢旧帧而不是积压。唤醒走 winit 的
//! [`winit::event_loop::EventLoopProxy`]（见 [`UiCommand`]）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use crate::frame::Frame;

/// 从任意线程发给 UI 事件循环的命令（`EventLoopProxy<UiCommand>` 的负载）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UiCommand {
    /// 邮箱里可能有新帧，请重绘。
    Frame,
    /// 帧来源已结束（channel 断开/迭代器耗尽）：投递 [`crate::ViewerEvent::Closed`] 后退出。
    FramesEnded,
    /// 调用方要求关闭（[`crate::PresenterHandle::close`]）。
    Close,
}

/// 只保留最新一帧的邮箱（可跨线程共享）。
#[derive(Debug, Default)]
pub(crate) struct FrameMailbox {
    latest: Mutex<Option<Frame>>,
    window_ready: AtomicBool,
}

impl FrameMailbox {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 覆盖式写入最新帧（旧帧被丢弃）。持锁时间仅是一次 `Option` 替换。
    pub(crate) fn store_frame(&self, frame: Frame) {
        let mut latest = self.latest.lock().unwrap_or_else(|err| err.into_inner());
        *latest = Some(frame);
    }

    /// 取走当前最新帧（UI 线程渲染前调用；没有新帧时返回 `None`）。
    #[must_use]
    pub(crate) fn take_latest(&self) -> Option<Frame> {
        let mut latest = self.latest.lock().unwrap_or_else(|err| err.into_inner());
        latest.take()
    }

    /// 窗口是否已创建完成。
    #[must_use]
    pub(crate) fn window_ready(&self) -> bool {
        self.window_ready.load(Ordering::Acquire)
    }

    /// 标记窗口已创建（只在 `resumed` 里调用一次）。
    pub(crate) fn set_window_ready(&self) {
        self.window_ready.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(width: u32, height: u32, fill: u8) -> Frame {
        Frame::new(
            width,
            height,
            vec![fill; width as usize * height as usize * 4],
        )
        .expect("测试帧尺寸合法")
    }

    #[test]
    fn mailbox_keeps_only_latest_frame() {
        let mailbox = FrameMailbox::new();
        assert!(mailbox.take_latest().is_none());

        mailbox.store_frame(frame(2, 2, 0x11));
        mailbox.store_frame(frame(3, 1, 0x22));
        let latest = mailbox.take_latest().expect("应有最新帧");
        assert_eq!(latest.size(), (3, 1));
        assert_eq!(latest.bgra(), &[0x22; 3 * 4]);
        // 取走之后就空了：旧帧已经被覆盖丢弃，不会排队。
        assert!(mailbox.take_latest().is_none());
    }

    #[test]
    fn window_ready_starts_false_and_sticks() {
        let mailbox = FrameMailbox::new();
        assert!(!mailbox.window_ready());
        mailbox.set_window_ready();
        assert!(mailbox.window_ready());
        mailbox.set_window_ready();
        assert!(mailbox.window_ready());
    }

    #[test]
    fn mailbox_accepts_pushes_from_many_threads() {
        let mailbox = std::sync::Arc::new(FrameMailbox::new());
        let mut handles = Vec::new();
        for thread_index in 0..4u8 {
            let mailbox = mailbox.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..32 {
                    mailbox.store_frame(frame(1, 1, thread_index));
                }
            }));
        }
        for handle in handles {
            handle.join().expect("推帧线程不应 panic");
        }
        // 并发覆盖之后仍应有且仅有一帧；内容来自某个线程。
        let latest = mailbox.take_latest().expect("至少保留最后一帧");
        assert_eq!(latest.byte_len(), 4);
        assert!(mailbox.take_latest().is_none());
    }
}
