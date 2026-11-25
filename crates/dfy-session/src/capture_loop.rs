// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 采集 + 编码线程：DXGI 采集对象与编码器都绑定创建线程，且都不是 `Send`，
//! 因此统一在独立 OS 线程里创建与运行，通过有界 channel 交给异步发送侧。

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::{Capturer, EncodedFrame, Encoder};

/// 采集线程配置。
#[derive(Clone, Debug)]
pub struct CaptureLoopConfig {
    /// 每次 `capture` 的等待上限（毫秒）；到点无新帧返回 `None`。
    pub capture_timeout_ms: u32,
    /// 帧率上限（0 = 不限）。
    pub max_fps: u32,
    /// 编码结果队列容量；队列满时丢弃新帧（实时优先，不阻塞采集）。
    pub queue_capacity: usize,
    /// 连续采集失败多少次后退出线程。
    pub max_consecutive_errors: u32,
}

impl Default for CaptureLoopConfig {
    fn default() -> Self {
        Self {
            capture_timeout_ms: 16,
            max_fps: 60,
            queue_capacity: 8,
            max_consecutive_errors: 30,
        }
    }
}

/// 采集线程统计。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CaptureStats {
    /// 成功采集的帧数。
    pub captured: u64,
    /// 成功编码次数。
    pub encoded: u64,
    /// 交给发送侧的码流份数。
    pub frames_yielded: u64,
    /// 码流总字节。
    pub bytes: u64,
    /// 关键帧数。
    pub keyframes: u64,
    /// 采集失败次数。
    pub capture_errors: u64,
    /// 编码失败次数。
    pub encode_errors: u64,
}

/// 已启动的采集线程句柄。
pub struct CaptureLoop {
    /// 编码结果队列（`None` = 线程结束）。
    pub frames: mpsc::Receiver<EncodedFrame>,
    stop_flag: Arc<AtomicBool>,
    join: Option<JoinHandle<anyhow::Result<CaptureStats>>>,
}

impl CaptureLoop {
    /// 请求线程停止（下一次循环检查时退出）。
    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::SeqCst);
    }

    /// 线程是否已请求停止。
    pub fn is_stopping(&self) -> bool {
        self.stop_flag.load(Ordering::SeqCst)
    }

    /// 等待线程结束并取回统计。`stop()` 后调用；重复调用返回 `None`。
    pub fn join(&mut self) -> Option<anyhow::Result<CaptureStats>> {
        self.join.take().map(|h| {
            h.join()
                .unwrap_or_else(|_| Err(anyhow::anyhow!("采集线程 panic")))
        })
    }
}

/// 在独立线程启动「采集 → 编码」循环。
///
/// `build` 在该线程内执行并创建采集器与编码器（因此可以创建 `!Send` 的对象，
/// 例如 DXGI/COM 相关的 `DxgiCapturer`）。
pub fn start_capture_loop<C, E, F>(cfg: CaptureLoopConfig, build: F) -> anyhow::Result<CaptureLoop>
where
    F: FnOnce() -> anyhow::Result<(C, E)> + Send + 'static,
    C: Capturer + 'static,
    E: Encoder + 'static,
{
    let (tx, rx) = mpsc::channel::<EncodedFrame>(cfg.queue_capacity.max(1));
    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop = Arc::clone(&stop_flag);

    let join = thread::Builder::new()
        .name("dfy-capture".into())
        .spawn(move || -> anyhow::Result<CaptureStats> {
            let (mut capturer, mut encoder) = build()?;
            let mut stats = CaptureStats::default();
            let mut consecutive_errors = 0u32;
            let min_interval = if cfg.max_fps == 0 {
                Duration::ZERO
            } else {
                Duration::from_secs_f64(1.0 / f64::from(cfg.max_fps))
            };
            let mut last_emit = Instant::now()
                .checked_sub(min_interval)
                .unwrap_or_else(Instant::now);

            while !stop.load(Ordering::SeqCst) {
                match capturer.capture(cfg.capture_timeout_ms) {
                    Ok(Some(frame)) => {
                        consecutive_errors = 0;
                        let now = Instant::now();
                        if now.duration_since(last_emit) < min_interval {
                            // 超出目标帧率：跳过（不进入编码，省 GPU/CPU）。
                            // 注意：Linux X11 GetImage 每次都会立即返回整帧，不做这段
                            // 补睡会以数百 fps 空转并整帧丢弃，因此必须睡完剩余间隔。
                            thread::sleep(min_interval - now.duration_since(last_emit));
                            continue;
                        }
                        last_emit = now;
                        stats.captured += 1;
                        match encoder.encode(&frame) {
                            Ok(outputs) => {
                                stats.encoded += 1;
                                for out in outputs {
                                    if out.keyframe {
                                        stats.keyframes += 1;
                                    }
                                    stats.bytes += out.data.len() as u64;
                                    stats.frames_yielded += 1;
                                    // 队列满说明发送侧跟不上：丢帧而不是阻塞采集
                                    if tx.try_send(out).is_err() {
                                        debug!("编码队列已满，丢弃一帧");
                                        break;
                                    }
                                }
                            }
                            Err(err) => {
                                stats.encode_errors += 1;
                                warn!(error = %err, "编码失败（继续下一帧）");
                            }
                        }
                    }
                    Ok(None) => {
                        // 超时无新帧：正常（静态桌面）
                    }
                    Err(err) => {
                        stats.capture_errors += 1;
                        consecutive_errors += 1;
                        warn!(error = %err, "采集失败");
                        if consecutive_errors >= cfg.max_consecutive_errors {
                            anyhow::bail!(
                                "连续 {} 次采集失败，退出采集线程：{err}",
                                consecutive_errors
                            );
                        }
                        thread::sleep(Duration::from_millis(100));
                    }
                }
            }
            Ok(stats)
        })
        .map_err(|e| anyhow::anyhow!("启动采集线程失败：{e}"))?;

    Ok(CaptureLoop {
        frames: rx,
        stop_flag,
        join: Some(join),
    })
}

impl Drop for CaptureLoop {
    fn drop(&mut self) {
        self.stop();
        if let Some(handle) = self.join.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{FakeCapturer, FakeEncoder};

    #[test]
    fn capture_loop_emits_frames() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let cfg = CaptureLoopConfig {
                capture_timeout_ms: 1,
                max_fps: 120,
                queue_capacity: 16,
                max_consecutive_errors: 3,
            };
            let mut capture = start_capture_loop(cfg, || {
                Ok((FakeCapturer::new(64, 48, u64::MAX), FakeEncoder::new()))
            })
            .unwrap();
            let mut got = 0;
            for _ in 0..8 {
                match tokio::time::timeout(Duration::from_secs(2), capture.frames.recv()).await {
                    Ok(Some(_)) => got += 1,
                    other => panic!("期望收到编码帧，实际 {other:?}"),
                }
            }
            assert_eq!(got, 8);
            capture.stop();
            let stats = capture.join().expect("线程已启动").expect("线程正常结束");
            assert!(stats.captured >= 8);
            assert!(stats.encoded >= 8);
        });
    }
}
