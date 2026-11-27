// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 采集 + 编码线程：DXGI 采集对象与编码器都绑定创建线程，且都不是 `Send`，
//! 因此统一在独立 OS 线程里创建与运行，通过有界 channel 交给异步发送侧。
//!
//! 运行时质量控制通过 [`CaptureRuntimeControl`] 发送命令。所有编码器重建、缩放和
//! 关键帧请求都只发生在采集线程；控制线程不会接触采集器或编码器对象。

use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::Context;
use tokio::sync::mpsc as tokio_mpsc;
use tracing::{debug, warn};

use crate::adaptation::AdaptationState;
use crate::{Capturer, EncodedFrame, Encoder, RawFrame};

/// 会话控制面支持的帧率范围。
pub const MIN_RUNTIME_FPS: u32 = 1;
/// 会话控制面支持的帧率范围。
pub const MAX_RUNTIME_FPS: u32 = 120;
/// 会话控制面支持的最小码率（kbps）。
pub const MIN_RUNTIME_BITRATE_KBPS: u32 = 100;
/// 会话控制面支持的最大码率（kbps）。
pub const MAX_RUNTIME_BITRATE_KBPS: u32 = 100_000;
/// 旧启动 API 没有编码器设置时的控制面初始码率（kbps）。
///
/// 新集成应使用 [`start_capture_loop_with_encoder_factory`] 并显式传入真实初始设置。
pub const DEFAULT_RUNTIME_BITRATE_KBPS: u32 = 4000;
const SETTINGS_REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// 编码分辨率限制。
///
/// 宽高均须为非零偶数。采集线程把它作为**包围盒**拟合当前采集尺寸，保持宽高比，
/// 且不会把采集画面放大。例如 1920×1080 的采集面使用 1280×700 的限制会得到
/// 1244×700（再向偶数取整）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodeResolution {
    /// 最大编码宽度（像素）。
    pub width: u32,
    /// 最大编码高度（像素）。
    pub height: u32,
}

impl EncodeResolution {
    /// 校验分辨率限制。
    pub fn validate(self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.width >= 2 && self.height >= 2,
            "dfy-session: 编码分辨率限制宽高必须至少为 2（{}x{}）",
            self.width,
            self.height
        );
        anyhow::ensure!(
            self.width % 2 == 0 && self.height % 2 == 0,
            "dfy-session: 编码分辨率限制宽高必须为偶数（{}x{}）",
            self.width,
            self.height
        );
        Ok(())
    }

    /// 按采集面宽高比拟合限制，不放大，并保证 NV12 所需的偶数尺寸。
    pub fn fit(self, source_width: u32, source_height: u32) -> anyhow::Result<Self> {
        self.validate()?;
        anyhow::ensure!(
            source_width > 0 && source_height > 0,
            "dfy-session: 采集尺寸不能为 0（{source_width}x{source_height}）"
        );
        let sw = u64::from(source_width);
        let sh = u64::from(source_height);
        let (mut width, mut height) = if u64::from(self.width) * sh <= u64::from(self.height) * sw {
            (u64::from(self.width), (u64::from(self.width) * sh) / sw)
        } else {
            ((u64::from(self.height) * sw) / sh, u64::from(self.height))
        };
        width = width.min(sw) & !1;
        height = height.min(sh) & !1;
        anyhow::ensure!(
            width >= 2 && height >= 2,
            "dfy-session: 分辨率限制相对于采集面过小（{}x{} → {width}x{height}）",
            source_width,
            source_height
        );
        Ok(Self {
            width: width as u32,
            height: height as u32,
        })
    }
}

/// 采集循环的运行时质量设置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeSettings {
    /// 目标帧率（1..=120）。
    pub fps: u32,
    /// 目标码率（kbps，100..=100_000）。
    pub bitrate_kbps: u32,
    /// 编码分辨率包围盒；`None` 表示使用采集面尺寸（不缩放）。
    pub resolution: Option<EncodeResolution>,
}

impl RuntimeSettings {
    /// 创建不缩放的控制面设置。
    pub const fn new(fps: u32, bitrate_kbps: u32) -> Self {
        Self {
            fps,
            bitrate_kbps,
            resolution: None,
        }
    }

    /// 校验所有运行时设置。
    pub fn validate(self) -> anyhow::Result<()> {
        anyhow::ensure!(
            (MIN_RUNTIME_FPS..=MAX_RUNTIME_FPS).contains(&self.fps),
            "dfy-session: fps 必须在 {MIN_RUNTIME_FPS}..={MAX_RUNTIME_FPS} 之间（收到 {}）",
            self.fps
        );
        anyhow::ensure!(
            (MIN_RUNTIME_BITRATE_KBPS..=MAX_RUNTIME_BITRATE_KBPS).contains(&self.bitrate_kbps),
            "dfy-session: 码率必须在 {MIN_RUNTIME_BITRATE_KBPS}..={MAX_RUNTIME_BITRATE_KBPS} kbps 之间（收到 {}）",
            self.bitrate_kbps
        );
        if let Some(resolution) = self.resolution {
            resolution.validate()?;
        }
        Ok(())
    }
}

impl CaptureHandle {
    pub(crate) fn control_refs(&self) -> CaptureControlRefs<'_> {
        CaptureControlRefs {
            stop: &self.stop_flag,
            keyframe: &self.keyframe_flag,
            runtime: &self.runtime,
            adaptation: &self.adaptation,
            active_sessions: &self.active_sessions,
            activity: &self.activity,
        }
    }
}

/// 采集线程配置。
#[derive(Clone, Debug)]
pub struct CaptureLoopConfig {
    /// 每次 `capture` 的等待上限（毫秒）；到点无新帧返回 `None`。
    pub capture_timeout_ms: u32,
    /// 旧启动 API 的初始帧率上限（1..=120；0 等价于 120）。带编码器工厂的
    /// 启动 API 使用调用方显式传入的 `initial.fps`。
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
    /// 成功应用的运行时设置命令数。
    pub settings_applied: u64,
    /// 设置命令失败数（包括不支持的控制和重建失败）。
    pub settings_errors: u64,
    /// 因运行时设置变化重建编码器的次数。
    pub encoder_rebuilds: u64,
    /// 最近一次设置错误；成功应用后清空。
    pub last_settings_error: Option<String>,
}

/// 外部编码器向采集循环注入码流的句柄。
#[derive(Clone)]
pub struct FrameInput {
    tx: tokio_mpsc::Sender<EncodedFrame>,
    stop: Arc<AtomicBool>,
    keyframe: Arc<AtomicBool>,
    stats: Arc<Mutex<CaptureStats>>,
    active_sessions: Arc<AtomicUsize>,
    pause_when_idle: Arc<AtomicBool>,
}

impl FrameInput {
    /// 提交一份码流；队列已满或采集循环停止时丢弃新帧。
    pub fn push(&self, frame: EncodedFrame) -> bool {
        if self.stop.load(Ordering::SeqCst) {
            return false;
        }
        if self.pause_when_idle.load(Ordering::SeqCst)
            && self.active_sessions.load(Ordering::SeqCst) == 0
        {
            return false;
        }
        let bytes = frame.data.len() as u64;
        let keyframe = frame.keyframe;
        if self.tx.try_send(frame).is_ok() {
            let mut stats = self
                .stats
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            stats.frames_yielded += 1;
            stats.bytes += bytes;
            if keyframe {
                stats.keyframes += 1;
            }
            true
        } else {
            false
        }
    }

    /// 取出并清除下一次编码需要的关键帧请求。
    pub fn take_keyframe_request(&self) -> bool {
        self.keyframe.swap(false, Ordering::SeqCst)
    }
}

enum RuntimeCommand {
    Apply {
        settings: RuntimeSettings,
        reply: Sender<anyhow::Result<RuntimeSettings>>,
        cancelled: Arc<Mutex<bool>>,
    },
    Shutdown,
}

type RuntimeSettingsApply =
    dyn Fn(RuntimeSettings) -> anyhow::Result<RuntimeSettings> + Send + 'static;

/// 可克隆、可跨线程使用的采集质量控制句柄。
///
/// [`apply`](Self::apply) 会等待采集线程确认设置已经生效；它不是异步方法，不应在
/// Tokio 工作线程上直接调用。请求超时返回明确错误；取消标志可阻止尚未开始处理的请求，
/// 但已经开始的重建无法安全中断。
#[derive(Clone)]
pub struct CaptureRuntimeControl {
    commands: Sender<RuntimeCommand>,
    current: Arc<Mutex<RuntimeSettings>>,
    source_resolution: Arc<Mutex<Option<EncodeResolution>>>,
}

impl CaptureRuntimeControl {
    fn new(initial: RuntimeSettings) -> (Self, Receiver<RuntimeCommand>) {
        Self::with_source_resolution(initial, initial.resolution)
    }

    fn with_source_resolution(
        initial: RuntimeSettings,
        source_resolution: Option<EncodeResolution>,
    ) -> (Self, Receiver<RuntimeCommand>) {
        let (commands, replies) = mpsc::channel();
        (
            Self {
                commands,
                current: Arc::new(Mutex::new(initial)),
                source_resolution: Arc::new(Mutex::new(source_resolution)),
            },
            replies,
        )
    }

    /// 读取最近一次成功生效的设置（或启动时的初始设置）。
    pub fn current(&self) -> RuntimeSettings {
        *self
            .current
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 读取采集面/平台视频源的原生尺寸；它与已降级的编码尺寸相互独立。
    pub fn source_resolution(&self) -> Option<EncodeResolution> {
        *self
            .source_resolution
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn publish_source_resolution(
        &self,
        resolution: Option<EncodeResolution>,
    ) -> anyhow::Result<()> {
        if let Some(resolution) = resolution {
            resolution.validate()?;
        }
        *self
            .source_resolution
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = resolution;
        Ok(())
    }

    /// 校验并应用设置，返回采集线程实际生效的归一化设置。
    pub fn apply(&self, settings: RuntimeSettings) -> anyhow::Result<RuntimeSettings> {
        settings.validate()?;
        let (reply, response) = mpsc::channel();
        let cancelled = Arc::new(Mutex::new(false));
        self.commands
            .send(RuntimeCommand::Apply {
                settings,
                reply,
                cancelled: Arc::clone(&cancelled),
            })
            .map_err(|err| anyhow::anyhow!("dfy-session: 采集线程不可用，无法应用设置：{err}"))?;
        match response.recv_timeout(SETTINGS_REPLY_TIMEOUT) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => {
                *cancelled
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
                anyhow::bail!(
                    "dfy-session: 等待运行时设置生效超时；请求可能已开始执行，请查询 current() 确认"
                )
            }
            Err(RecvTimeoutError::Disconnected) => {
                anyhow::bail!("dfy-session: 采集线程在应用设置时退出")
            }
        }
    }
}

type CaptureActivityCallback = dyn Fn(bool) + Send + Sync;

struct CaptureActivityTransition {
    sessions: usize,
    observer: Option<Arc<CaptureActivityCallback>>,
    stopped: bool,
}

pub(crate) struct CaptureActivityState {
    active_sessions: Arc<AtomicUsize>,
    transition: Mutex<CaptureActivityTransition>,
}

impl CaptureActivityState {
    fn new(active_sessions: Arc<AtomicUsize>) -> Self {
        Self {
            active_sessions,
            transition: Mutex::new(CaptureActivityTransition {
                sessions: 0,
                observer: None,
                stopped: false,
            }),
        }
    }

    fn set_observer(&self, observer: Arc<CaptureActivityCallback>) {
        let mut transition = self
            .transition
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let active = !transition.stopped && transition.sessions > 0;
        transition.observer = Some(observer);
        if active {
            notify_capture_activity(&mut transition, true);
        }
    }

    pub(crate) fn stop(&self, stop_flag: &AtomicBool) {
        let mut transition = self
            .transition
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        transition.stopped = true;
        stop_flag.store(true, Ordering::SeqCst);
    }
}

fn notify_capture_activity(transition: &mut CaptureActivityTransition, active: bool) {
    if let Some(observer) = &transition.observer {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| observer(active)));
    }
}

/// 已启动的采集线程句柄。
pub struct CaptureLoop {
    /// 编码结果队列（`None` = 线程结束）。
    pub frames: tokio_mpsc::Receiver<EncodedFrame>,
    pub(crate) stop_flag: Arc<AtomicBool>,
    pub(crate) keyframe_flag: Arc<AtomicBool>,
    pub(crate) runtime: CaptureRuntimeControl,
    pub(crate) adaptation: Arc<tokio::sync::Mutex<AdaptationState>>,
    pub(crate) active_sessions: Arc<AtomicUsize>,
    pub(crate) activity: Arc<CaptureActivityState>,
    pub(crate) pause_when_idle: Arc<AtomicBool>,
    join: Option<JoinHandle<anyhow::Result<CaptureStats>>>,
    runtime_join: Option<JoinHandle<()>>,
    stats: Arc<Mutex<CaptureStats>>,
}

pub(crate) struct CaptureActivityGuard {
    activity: Arc<CaptureActivityState>,
    active: bool,
}

impl CaptureActivityGuard {
    pub(crate) fn acquire_if_running(
        active_sessions: &Arc<AtomicUsize>,
        activity: &Arc<CaptureActivityState>,
    ) -> Option<Self> {
        let mut transition = activity
            .transition
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if transition.stopped {
            return None;
        }
        active_sessions.fetch_add(1, Ordering::SeqCst);
        let first = transition.sessions == 0;
        transition.sessions += 1;
        if first {
            notify_capture_activity(&mut transition, true);
        }
        Some(Self {
            activity: Arc::clone(activity),
            active: true,
        })
    }

    pub(crate) fn acquire_pending_if_running(activity: &Arc<CaptureActivityState>) -> Option<Self> {
        let mut transition = activity
            .transition
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if transition.stopped {
            return None;
        }
        let first = transition.sessions == 0;
        transition.sessions += 1;
        if first {
            notify_capture_activity(&mut transition, true);
        }
        Some(Self {
            activity: Arc::clone(activity),
            active: false,
        })
    }

    pub(crate) fn activate(&mut self) -> bool {
        if self.active {
            return true;
        }
        let transition = self
            .activity
            .transition
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if transition.stopped {
            return false;
        }
        self.activity.active_sessions.fetch_add(1, Ordering::SeqCst);
        self.active = true;
        true
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active
    }

    pub(crate) fn deactivate(&mut self) {
        if !self.active {
            return;
        }
        let _transition = self
            .activity
            .transition
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.activity.active_sessions.fetch_sub(1, Ordering::SeqCst);
        self.active = false;
    }
}

impl Drop for CaptureActivityGuard {
    fn drop(&mut self) {
        let mut transition = self
            .activity
            .transition
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.active {
            self.activity.active_sessions.fetch_sub(1, Ordering::SeqCst);
        }
        transition.sessions -= 1;
        if transition.sessions == 0 {
            notify_capture_activity(&mut transition, false);
        }
    }
}

/// 可跨并发连接共享的采集队列与控制标志；不拥有采集线程的 join 句柄。
#[derive(Clone)]
pub struct CaptureHandle {
    pub(crate) frames: Arc<tokio::sync::Mutex<tokio_mpsc::Receiver<EncodedFrame>>>,
    pub(crate) stop_flag: Arc<AtomicBool>,
    pub(crate) keyframe_flag: Arc<AtomicBool>,
    pub(crate) runtime: CaptureRuntimeControl,
    pub(crate) adaptation: Arc<tokio::sync::Mutex<AdaptationState>>,
    pub(crate) active_sessions: Arc<AtomicUsize>,
    pub(crate) activity: Arc<CaptureActivityState>,
    pub(crate) pause_when_idle: Arc<AtomicBool>,
}

pub(crate) struct CaptureControlRefs<'a> {
    pub stop: &'a AtomicBool,
    pub keyframe: &'a AtomicBool,
    pub runtime: &'a CaptureRuntimeControl,
    pub adaptation: &'a tokio::sync::Mutex<AdaptationState>,
    pub active_sessions: &'a Arc<AtomicUsize>,
    pub activity: &'a Arc<CaptureActivityState>,
}

impl CaptureLoop {
    /// 取出队列供多个并发会话共享；原对象继续负责停止与 join 采集线程。
    pub fn shared_handle(&mut self) -> CaptureHandle {
        let (_closed_tx, closed_rx) = tokio_mpsc::channel(1);
        let frames = std::mem::replace(&mut self.frames, closed_rx);
        CaptureHandle {
            frames: Arc::new(tokio::sync::Mutex::new(frames)),
            stop_flag: Arc::clone(&self.stop_flag),
            keyframe_flag: Arc::clone(&self.keyframe_flag),
            runtime: self.runtime.clone(),
            adaptation: Arc::clone(&self.adaptation),
            active_sessions: Arc::clone(&self.active_sessions),
            activity: Arc::clone(&self.activity),
            pause_when_idle: Arc::clone(&self.pause_when_idle),
        }
    }

    /// 接入平台侧已经编码好的码流，不启动 Rust 采集线程。
    ///
    /// 外部码流源可以使用关键帧/停止标志，但分辨率与编码器控制会明确返回不支持。
    pub fn external(queue_capacity: usize) -> (Self, FrameInput) {
        let (tx, frames) = tokio_mpsc::channel(queue_capacity.max(1));
        let stop_flag = Arc::new(AtomicBool::new(false));
        let keyframe_flag = Arc::new(AtomicBool::new(false));
        let active_sessions = Arc::new(AtomicUsize::new(0));
        let activity = Arc::new(CaptureActivityState::new(Arc::clone(&active_sessions)));
        let pause_when_idle = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(Mutex::new(CaptureStats::default()));
        let input = FrameInput {
            tx,
            stop: Arc::clone(&stop_flag),
            keyframe: Arc::clone(&keyframe_flag),
            stats: Arc::clone(&stats),
            active_sessions: Arc::clone(&active_sessions),
            pause_when_idle: Arc::clone(&pause_when_idle),
        };
        let (runtime, _replies) = CaptureRuntimeControl::new(RuntimeSettings::new(
            MAX_RUNTIME_FPS,
            DEFAULT_RUNTIME_BITRATE_KBPS,
        ));
        let adaptation = AdaptationState::new(runtime.current());
        (
            Self {
                frames,
                stop_flag,
                keyframe_flag,
                runtime,
                adaptation: Arc::new(tokio::sync::Mutex::new(adaptation)),
                active_sessions,
                activity,
                pause_when_idle,
                join: None,
                runtime_join: None,
                stats,
            },
            input,
        )
    }

    /// 接入平台编码器，并在独立 OS 线程中处理运行时设置命令。
    ///
    /// `initial` 必须是平台编码器已经实际采用的设置。`apply` 只能由命令线程调用；
    /// 平台应返回实际生效的设置（分辨率 `None` 表示原生采集尺寸）。只有校验成功的
    /// 返回值才会发布给 [`CaptureRuntimeControl::current`]。失败、无效返回值或取消
    /// 都通过 reply 返回错误，不会关闭命令通道或改变已确认的设置。
    ///
    /// [`CaptureRuntimeControl::apply`] 是阻塞 API；Tokio 调用方应使用
    /// `spawn_blocking`（现有会话适配逻辑已经如此处理）。
    pub fn external_with_runtime<F>(
        queue_capacity: usize,
        initial: RuntimeSettings,
        apply: F,
    ) -> anyhow::Result<(Self, FrameInput)>
    where
        F: Fn(RuntimeSettings) -> anyhow::Result<RuntimeSettings> + Send + 'static,
    {
        Self::external_with_runtime_and_source(queue_capacity, initial, initial.resolution, apply)
    }

    /// 接入平台编码器，并显式提供与已应用编码尺寸独立的视频源尺寸。
    ///
    /// 平台在降级后重建时仍应传入原始视频源尺寸，而不是当前较小的编码尺寸。
    /// [`Self::external_with_runtime`] 为兼容调用方，默认把 `initial.resolution` 当作源尺寸。
    pub fn external_with_runtime_and_source<F>(
        queue_capacity: usize,
        initial: RuntimeSettings,
        source_resolution: Option<EncodeResolution>,
        apply: F,
    ) -> anyhow::Result<(Self, FrameInput)>
    where
        F: Fn(RuntimeSettings) -> anyhow::Result<RuntimeSettings> + Send + 'static,
    {
        initial.validate()?;
        if let Some(resolution) = source_resolution {
            resolution.validate()?;
        }
        let (tx, frames) = tokio_mpsc::channel(queue_capacity.max(1));
        let stop_flag = Arc::new(AtomicBool::new(false));
        let keyframe_flag = Arc::new(AtomicBool::new(false));
        let active_sessions = Arc::new(AtomicUsize::new(0));
        let activity = Arc::new(CaptureActivityState::new(Arc::clone(&active_sessions)));
        let pause_when_idle = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(Mutex::new(CaptureStats::default()));
        let input = FrameInput {
            tx,
            stop: Arc::clone(&stop_flag),
            keyframe: Arc::clone(&keyframe_flag),
            stats: Arc::clone(&stats),
            active_sessions: Arc::clone(&active_sessions),
            pause_when_idle: Arc::clone(&pause_when_idle),
        };
        let (runtime, replies) =
            CaptureRuntimeControl::with_source_resolution(initial, source_resolution);
        let adaptation = AdaptationState::new(initial);
        let runtime_for_thread = runtime.clone();
        let stop_for_thread = Arc::clone(&stop_flag);
        let apply: Box<RuntimeSettingsApply> = Box::new(apply);
        let runtime_join = thread::Builder::new()
            .name("dfy-external-runtime".into())
            .spawn(move || {
                let mut current = initial;
                while !stop_for_thread.load(Ordering::SeqCst) {
                    let command = match replies.recv_timeout(Duration::from_millis(100)) {
                        Ok(command) => command,
                        Err(RecvTimeoutError::Timeout) => continue,
                        Err(RecvTimeoutError::Disconnected) => break,
                    };
                    match command {
                        RuntimeCommand::Shutdown => break,
                        RuntimeCommand::Apply {
                            settings,
                            reply,
                            cancelled,
                        } => {
                            let result = apply_external_settings(
                                settings,
                                &cancelled,
                                apply.as_ref(),
                                &mut current,
                                &runtime_for_thread,
                            );
                            // 调用方超时后可能已经离开；发送失败不代表应用失败。
                            let _ = reply.send(result);
                        }
                    }
                }
            })
            .map_err(|error| anyhow::anyhow!("启动外部编码器控制线程失败：{error}"))?;
        Ok((
            Self {
                frames,
                stop_flag,
                keyframe_flag,
                runtime,
                adaptation: Arc::new(tokio::sync::Mutex::new(adaptation)),
                active_sessions,
                activity,
                pause_when_idle,
                join: None,
                runtime_join: Some(runtime_join),
                stats,
            },
            input,
        ))
    }

    /// 请求线程停止（下一次循环检查时退出）。
    pub fn stop(&self) {
        self.activity.stop(&self.stop_flag);
        // The loop owns a clone of the control sender; shutdown must be explicit.
        // Capture loops simply ignore this queued marker when they next poll commands.
        let _ = self.runtime.commands.send(RuntimeCommand::Shutdown);
    }

    /// 请求编码器在下一帧强制关键帧（新连接接入 / 路径迁移后调用）。
    pub fn request_keyframe(&self) {
        self.keyframe_flag.store(true, Ordering::SeqCst);
    }

    /// 获取可克隆的运行时质量控制句柄。
    pub fn runtime_control(&self) -> CaptureRuntimeControl {
        self.runtime.clone()
    }

    /// 观察认证会话的总数是否从零变为非零或回到零。
    ///
    /// 待提交迁移连接也计入观察状态，避免旧连接断开时误停平台编码器；它们仍不会
    /// 放行帧或应用画质。回调在状态转换锁内同步调用，必须简短、非阻塞且不可重入。
    pub fn set_capture_activity_observer<F>(&self, observer: F)
    where
        F: Fn(bool) + Send + Sync + 'static,
    {
        self.activity
            .set_observer(Arc::new(observer) as Arc<CaptureActivityCallback>);
    }

    /// 请求应用 fps/码率/分辨率，并等待采集线程确认。
    pub fn apply_runtime_settings(
        &self,
        settings: RuntimeSettings,
    ) -> anyhow::Result<RuntimeSettings> {
        self.runtime.apply(settings)
    }

    /// 线程是否已请求停止。
    pub fn is_stopping(&self) -> bool {
        self.stop_flag.load(Ordering::SeqCst)
    }

    /// 读取采集与编码的实时累计统计，不等待线程退出。
    pub fn stats(&self) -> CaptureStats {
        self.stats
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// 等待线程结束并取回统计。`stop()` 后调用；重复调用返回 `None`。
    pub fn join(&mut self) -> Option<anyhow::Result<CaptureStats>> {
        if let Some(handle) = self.runtime_join.take() {
            let _ = handle.join();
        }
        self.join.take().map(|h| {
            h.join()
                .unwrap_or_else(|_| Err(anyhow::anyhow!("采集线程 panic")))
        })
    }
}

/// 在独立线程启动「采集 → 编码」循环。
///
/// `build` 在该线程内执行并创建采集器与编码器（因此可以创建 `!Send` 的对象，
/// 例如 DXGI/COM 相关的 `DxgiCapturer`）。分辨率变化需要编码器工厂；此兼容入口
/// 不提供工厂，遇到分辨率变化会返回明确错误。fps/码率则调用编码器动态控制接口。
pub fn start_capture_loop<C, E, F>(cfg: CaptureLoopConfig, build: F) -> anyhow::Result<CaptureLoop>
where
    F: FnOnce() -> anyhow::Result<(C, E)> + Send + 'static,
    C: Capturer + 'static,
    E: Encoder + 'static,
{
    let fps = normalized_fps(cfg.max_fps);
    start_capture_loop_internal(
        cfg,
        RuntimeSettings::new(fps, DEFAULT_RUNTIME_BITRATE_KBPS),
        build,
        None,
    )
}

/// 启动带编码器工厂的采集循环，支持安全重建以应用运行时设置。
///
/// `initial` 必须是调用方启动编码器时使用的真实设置。`rebuild` 在采集线程内执行，
/// 接收包含归一化分辨率、fps 和码率的完整设置，并返回新编码器。分辨率变化或编码器
/// 不支持动态控制时会走此工厂。只有工厂成功后才替换旧编码器；失败时旧编码器继续可用。
/// 重建前会调用旧编码器的 `flush`，旧参数的缓冲输出被有意丢弃，以免与新参数集交错；
/// 新编码器下一帧会被请求为关键帧。
pub fn start_capture_loop_with_encoder_factory<C, E, F, G>(
    cfg: CaptureLoopConfig,
    initial: RuntimeSettings,
    build: F,
    rebuild: G,
) -> anyhow::Result<CaptureLoop>
where
    F: FnOnce() -> anyhow::Result<(C, E)> + Send + 'static,
    G: FnMut(&RuntimeSettings) -> anyhow::Result<E> + Send + 'static,
    C: Capturer + 'static,
    E: Encoder + 'static,
{
    initial.validate()?;
    let configured_fps = normalized_fps(cfg.max_fps);
    anyhow::ensure!(
        (MIN_RUNTIME_FPS..=MAX_RUNTIME_FPS).contains(&configured_fps),
        "dfy-session: CaptureLoopConfig.max_fps 必须在 {MIN_RUNTIME_FPS}..={MAX_RUNTIME_FPS} 之间（收到 {}）",
        cfg.max_fps
    );
    start_capture_loop_internal(cfg, initial, build, Some(Box::new(rebuild)))
}

type EncoderFactory<E> = Box<dyn FnMut(&RuntimeSettings) -> anyhow::Result<E> + Send>;

fn start_capture_loop_internal<C, E, F>(
    cfg: CaptureLoopConfig,
    initial: RuntimeSettings,
    build: F,
    rebuild: Option<EncoderFactory<E>>,
) -> anyhow::Result<CaptureLoop>
where
    F: FnOnce() -> anyhow::Result<(C, E)> + Send + 'static,
    C: Capturer + 'static,
    E: Encoder + 'static,
{
    initial.validate()?;
    let (tx, rx) = tokio_mpsc::channel::<EncodedFrame>(cfg.queue_capacity.max(1));
    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop = Arc::clone(&stop_flag);
    let keyframe_flag = Arc::new(AtomicBool::new(false));
    let keyframe = Arc::clone(&keyframe_flag);
    let active_sessions = Arc::new(AtomicUsize::new(0));
    let activity = Arc::new(CaptureActivityState::new(Arc::clone(&active_sessions)));
    let active_sessions_for_thread = Arc::clone(&active_sessions);
    let pause_when_idle = Arc::new(AtomicBool::new(false));
    let pause_when_idle_for_thread = Arc::clone(&pause_when_idle);
    let (runtime, replies) = CaptureRuntimeControl::new(initial);
    let adaptation = AdaptationState::new(initial);
    let runtime_for_thread = runtime.clone();
    let stats_snapshot = Arc::new(Mutex::new(CaptureStats::default()));
    let stats_for_thread = Arc::clone(&stats_snapshot);

    let join = thread::Builder::new()
        .name("dfy-capture".into())
        .spawn(move || -> anyhow::Result<CaptureStats> {
            let (mut capturer, mut encoder) = build()?;
            let mut stats = CaptureStats::default();
            let mut consecutive_errors = 0u32;
            let mut current = initial;
            let mut source_size = capturer.dimensions();
            let mut active_resolution = match source_size {
                Some((width, height)) => even_source_size(width, height)?,
                None => None,
            };
            if let Some(settings) = encoder.runtime_settings() {
                settings.validate()?;
                if current.resolution.is_none() {
                    current.bitrate_kbps = settings.bitrate_kbps;
                }
                if let Some(resolution) = settings.resolution {
                    resolution.validate()?;
                    active_resolution = Some(resolution);
                    current.resolution = Some(resolution);
                }
            }
            if let (Some((width, height)), Some(resolution)) = (source_size, current.resolution) {
                active_resolution = Some(resolution.fit(width, height)?);
                current.resolution = active_resolution;
            }
            publish_current(&runtime_for_thread, current);
            let mut rebuild = rebuild;
            let mut scaler = FrameScaler::default();
            let mut next_emit = Instant::now();

            while !stop.load(Ordering::SeqCst) {
                process_runtime_commands(
                    &replies,
                    &mut encoder,
                    &mut rebuild,
                    &mut current,
                    &mut active_resolution,
                    source_size,
                    &runtime_for_thread,
                    &mut stats,
                );
                if pause_when_idle_for_thread.load(Ordering::SeqCst)
                    && active_sessions_for_thread.load(Ordering::SeqCst) == 0
                {
                    thread::sleep(Duration::from_millis(20));
                    continue;
                }
                let captured = capturer.capture(cfg.capture_timeout_ms);
                if capturer.take_recovery_request() {
                    keyframe.store(true, Ordering::SeqCst);
                }
                match captured {
                    Ok(Some(frame)) => {
                        consecutive_errors = 0;
                        if source_size.is_none() {
                            source_size = Some((frame.width, frame.height));
                            let native = even_source_size(frame.width, frame.height)?;
                            if let Some(limit) = current.resolution {
                                let target = limit.fit(frame.width, frame.height)?;
                                if active_resolution != Some(target) {
                                    let requested = current;
                                    let applied = apply_settings(
                                        &mut encoder,
                                        rebuild.as_mut(),
                                        &mut current,
                                        &mut active_resolution,
                                        source_size,
                                        requested,
                                        &mut stats,
                                    )?;
                                    current = applied;
                                    publish_current(&runtime_for_thread, current);
                                }
                                active_resolution = Some(target);
                            } else {
                                active_resolution = native;
                                current.resolution = native;
                                publish_current(&runtime_for_thread, current);
                            }
                        }
                        let now = Instant::now();
                        if now < next_emit {
                            // X11 GetImage 会立即返回；必须睡完剩余间隔，不能忙等。
                            thread::sleep(next_emit.duration_since(now));
                            continue;
                        }
                        // 采集等待期间到达的设置先于本帧编码生效。
                        process_runtime_commands(
                            &replies,
                            &mut encoder,
                            &mut rebuild,
                            &mut current,
                            &mut active_resolution,
                            source_size,
                            &runtime_for_thread,
                            &mut stats,
                        );
                        next_emit = Instant::now() + frame_interval(current.fps);
                        stats.captured += 1;
                        let target = active_resolution
                            .expect("source size or encoder resolution initialized");
                        let encode_frame =
                            if target.width == frame.width && target.height == frame.height {
                                &frame
                            } else {
                                scaler.scale(&frame, target)?
                            };
                        if keyframe.swap(false, Ordering::SeqCst) {
                            encoder.request_keyframe();
                        }
                        match encoder.encode(encode_frame) {
                            Ok(outputs) => {
                                stats.encoded += 1;
                                for out in outputs {
                                    if out.keyframe {
                                        stats.keyframes += 1;
                                    }
                                    stats.bytes += out.data.len() as u64;
                                    if tx.try_send(out).is_err() {
                                        debug!("编码队列已满，丢弃一帧");
                                        break;
                                    }
                                    stats.frames_yielded += 1;
                                }
                            }
                            Err(err) => {
                                stats.encode_errors += 1;
                                warn!(error = %err, "编码失败（继续下一帧）");
                            }
                        }
                    }
                    Ok(None) => {
                        // 超时无新帧：正常（静态桌面）。
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
                *stats_for_thread
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = stats.clone();
            }
            Ok(stats)
        })
        .map_err(|e| anyhow::anyhow!("启动采集线程失败：{e}"))?;

    Ok(CaptureLoop {
        frames: rx,
        stop_flag,
        keyframe_flag,
        runtime,
        adaptation: Arc::new(tokio::sync::Mutex::new(adaptation)),
        active_sessions,
        activity,
        pause_when_idle,
        join: Some(join),
        runtime_join: None,
        stats: stats_snapshot,
    })
}

fn runtime_request_cancelled(cancelled: &Mutex<bool>) -> bool {
    *cancelled
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn apply_external_settings(
    requested: RuntimeSettings,
    cancelled: &Mutex<bool>,
    apply: &RuntimeSettingsApply,
    current: &mut RuntimeSettings,
    control: &CaptureRuntimeControl,
) -> anyhow::Result<RuntimeSettings> {
    let cancelled_error = || anyhow::anyhow!("dfy-session: 运行时设置请求已取消");
    if runtime_request_cancelled(cancelled) {
        return Err(cancelled_error());
    }
    requested.validate()?;
    let applied = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| apply(requested)))
    {
        Ok(result) => result?,
        Err(_) => anyhow::bail!("dfy-session: 平台运行时设置回调 panic"),
    };
    applied
        .validate()
        .context("dfy-session: 平台返回的运行时设置无效")?;
    let cancellation = cancelled
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if *cancellation {
        return Err(cancelled_error());
    }
    *current = applied;
    publish_current(control, applied);
    Ok(applied)
}

fn normalized_fps(max_fps: u32) -> u32 {
    if max_fps == 0 {
        MAX_RUNTIME_FPS
    } else {
        max_fps
    }
}

fn frame_interval(fps: u32) -> Duration {
    Duration::from_nanos(1_000_000_000 / u64::from(fps.max(1)))
}

fn even_source_size(width: u32, height: u32) -> anyhow::Result<Option<EncodeResolution>> {
    if width == 0 || height == 0 {
        anyhow::bail!("dfy-session: 采集尺寸不能为 0（{width}x{height}）");
    }
    let width = width & !1;
    let height = height & !1;
    if width < 2 || height < 2 {
        anyhow::bail!("dfy-session: 采集尺寸过小，无法编码 NV12（{width}x{height}）");
    }
    Ok(Some(EncodeResolution { width, height }))
}

fn publish_current(control: &CaptureRuntimeControl, settings: RuntimeSettings) {
    *control
        .current
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = settings;
}

#[allow(clippy::too_many_arguments)]
fn process_runtime_commands<E: Encoder>(
    replies: &Receiver<RuntimeCommand>,
    encoder: &mut E,
    rebuild: &mut Option<EncoderFactory<E>>,
    current: &mut RuntimeSettings,
    active_resolution: &mut Option<EncodeResolution>,
    source_size: Option<(u32, u32)>,
    control: &CaptureRuntimeControl,
    stats: &mut CaptureStats,
) {
    while let Ok(command) = replies.try_recv() {
        let RuntimeCommand::Apply {
            settings,
            reply,
            cancelled,
        } = command
        else {
            continue;
        };
        let mut result = if runtime_request_cancelled(&cancelled) {
            Err(anyhow::anyhow!("dfy-session: 运行时设置请求已取消"))
        } else {
            apply_settings(
                encoder,
                rebuild.as_mut(),
                current,
                active_resolution,
                source_size,
                settings,
                stats,
            )
        };
        if let Ok(applied) = result {
            let cancellation = cancelled
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if *cancellation {
                result = Err(anyhow::anyhow!("dfy-session: 运行时设置请求已取消"));
            } else {
                *current = applied;
                publish_current(control, applied);
                stats.settings_applied += 1;
                stats.last_settings_error = None;
            }
        }
        match &result {
            Ok(_) => {}
            Err(err) => {
                stats.settings_errors += 1;
                stats.last_settings_error = Some(err.to_string());
                warn!(error = %err, "运行时设置应用失败，本次设置未确认生效");
            }
        }
        let _ = reply.send(result);
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_settings<E: Encoder>(
    encoder: &mut E,
    mut rebuild: Option<&mut EncoderFactory<E>>,
    current: &mut RuntimeSettings,
    active_resolution: &mut Option<EncodeResolution>,
    source_size: Option<(u32, u32)>,
    mut requested: RuntimeSettings,
    stats: &mut CaptureStats,
) -> anyhow::Result<RuntimeSettings> {
    requested.validate()?;
    let (source_width, source_height) = source_size.ok_or_else(|| {
        anyhow::anyhow!(
            "dfy-session: 采集尺寸未知，暂不能应用编码分辨率；请先提供一帧或 Capturer::dimensions"
        )
    })?;
    let target = match requested.resolution {
        Some(limit) => limit.fit(source_width, source_height)?,
        None => even_source_size(source_width, source_height)?.expect("nonzero source"),
    };
    requested.resolution = Some(target);
    let resolution_changed = *active_resolution != Some(target);

    if resolution_changed {
        let factory = rebuild.ok_or_else(|| {
            anyhow::anyhow!(
                "dfy-session: 当前采集循环没有编码器工厂，不能安全重建以改变编码分辨率；使用 start_capture_loop_with_encoder_factory"
            )
        })?;
        replace_encoder(encoder, factory, &requested, "编码分辨率重建", stats)?;
        *active_resolution = Some(target);
        return Ok(requested);
    }

    let old_fps = current.fps;
    let mut dynamic_error = None;
    if requested.fps != current.fps {
        if let Err(err) = encoder.set_frame_rate(requested.fps) {
            dynamic_error = Some(err.context("dfy-session: 动态帧率调整失败"));
        }
    }
    if dynamic_error.is_none() && requested.bitrate_kbps != current.bitrate_kbps {
        if let Err(err) = encoder.set_bitrate(requested.bitrate_kbps) {
            if requested.fps != old_fps {
                if let Err(rollback_err) = encoder.set_frame_rate(old_fps) {
                    warn!(error = %rollback_err, "码率调整失败后回滚帧率也失败");
                    dynamic_error = Some(anyhow::anyhow!(
                        "dfy-session: 动态码率调整失败，且帧率回滚也失败：{err:#}；回滚错误：{rollback_err:#}"
                    ));
                }
            }
            if dynamic_error.is_none() {
                dynamic_error = Some(err.context("dfy-session: 动态码率调整失败"));
            }
        }
    }

    if let Some(err) = dynamic_error {
        if let Some(factory) = rebuild.as_mut() {
            match replace_encoder(
                encoder,
                factory,
                &requested,
                "动态控制失败后的兜底重建",
                stats,
            ) {
                Ok(()) => return Ok(requested),
                Err(rebuild_err) => {
                    return Err(err).context(format!(
                        "dfy-session: 动态控制失败后的兜底重建也失败：{rebuild_err:#}"
                    ));
                }
            }
        }
        return Err(err);
    }
    Ok(requested)
}

fn replace_encoder<E: Encoder>(
    encoder: &mut E,
    factory: &mut EncoderFactory<E>,
    requested: &RuntimeSettings,
    operation: &str,
    stats: &mut CaptureStats,
) -> anyhow::Result<()> {
    // 先冲刷旧 transform。若失败则不替换编码器，也不会丢掉旧编码器。
    encoder
        .flush()
        .context(format!("dfy-session: {operation}前冲刷编码器失败"))?;
    let replacement = factory(requested).context(format!(
        "dfy-session: {operation}失败，保留旧编码器；原始错误见来源链"
    ))?;
    *encoder = replacement;
    encoder.request_keyframe();
    stats.encoder_rebuilds += 1;
    Ok(())
}

/// 可复用的 BGRA8 双线性缩放器。输出始终为紧密排列的 `width * 4` 行跨距。
#[derive(Debug)]
struct FrameScaler {
    output: RawFrame,
}

impl Default for FrameScaler {
    fn default() -> Self {
        Self {
            output: RawFrame {
                width: 0,
                height: 0,
                stride: 0,
                data: Vec::new(),
                pts_us: 0,
                full_frame: true,
            },
        }
    }
}

impl FrameScaler {
    fn scale(&mut self, frame: &RawFrame, target: EncodeResolution) -> anyhow::Result<&RawFrame> {
        target.validate()?;
        anyhow::ensure!(
            frame.width > 0 && frame.height > 0,
            "dfy-session: 不能缩放空帧（{}x{}）",
            frame.width,
            frame.height
        );
        let src_stride = frame.stride as usize;
        let row_bytes = (frame.width as usize)
            .checked_mul(4)
            .ok_or_else(|| anyhow::anyhow!("dfy-session: 采集行宽溢出"))?;
        anyhow::ensure!(
            src_stride >= row_bytes,
            "dfy-session: 采集 stride {} 小于行宽 {row_bytes}",
            frame.stride
        );
        let needed = (frame.height as usize - 1)
            .checked_mul(src_stride)
            .and_then(|v| v.checked_add(row_bytes))
            .ok_or_else(|| anyhow::anyhow!("dfy-session: 采集缓冲长度溢出"))?;
        anyhow::ensure!(
            frame.data.len() >= needed,
            "dfy-session: 采集缓冲太小，需要 {needed} 字节，实得 {}",
            frame.data.len()
        );
        let dst_stride = (target.width as usize)
            .checked_mul(4)
            .ok_or_else(|| anyhow::anyhow!("dfy-session: 编码行宽溢出"))?;
        let dst_len = dst_stride
            .checked_mul(target.height as usize)
            .ok_or_else(|| anyhow::anyhow!("dfy-session: 缩放缓冲长度溢出"))?;
        self.output.width = target.width;
        self.output.height = target.height;
        self.output.stride = target.width * 4;
        self.output.pts_us = frame.pts_us;
        self.output.full_frame = frame.full_frame;
        self.output.data.resize(dst_len, 0);

        let sw = u64::from(frame.width);
        let sh = u64::from(frame.height);
        let dw = u64::from(target.width);
        let dh = u64::from(target.height);
        for y in 0..target.height as usize {
            let y_num = (2 * y as u64 + 1) * sh;
            let mut y0 = y_num / (2 * dh);
            let mut fy = ((y_num % (2 * dh)) << 16) / (2 * dh);
            if y0 + 1 >= sh {
                y0 = sh - 1;
                fy = 0;
            }
            let wy = fy;
            let iy = 65536 - wy;
            for x in 0..target.width as usize {
                let x_num = (2 * x as u64 + 1) * sw;
                let mut x0 = x_num / (2 * dw);
                let mut fx = ((x_num % (2 * dw)) << 16) / (2 * dw);
                if x0 + 1 >= sw {
                    x0 = sw - 1;
                    fx = 0;
                }
                let wx = fx;
                let ix = 65536 - wx;
                let w00 = ix * iy;
                let w10 = wx * iy;
                let w01 = ix * wy;
                let w11 = wx * wy;
                let row0 = y0 as usize * src_stride;
                let row1 = if y0 + 1 < sh {
                    (y0 as usize + 1) * src_stride
                } else {
                    row0
                };
                let col0 = x0 as usize * 4;
                let col1 = if x0 + 1 < sw {
                    (x0 as usize + 1) * 4
                } else {
                    col0
                };
                let dst = y * dst_stride + x * 4;
                for channel in 0..3 {
                    let p00 = u64::from(frame.data[row0 + col0 + channel]);
                    let p10 = u64::from(frame.data[row0 + col1 + channel]);
                    let p01 = u64::from(frame.data[row1 + col0 + channel]);
                    let p11 = u64::from(frame.data[row1 + col1 + channel]);
                    self.output.data[dst + channel] =
                        ((p00 * w00 + p10 * w10 + p01 * w01 + p11 * w11 + (1 << 31)) >> 32) as u8;
                }
                self.output.data[dst + 3] = 255;
            }
        }
        Ok(&self.output)
    }
}

impl Drop for CaptureLoop {
    fn drop(&mut self) {
        self.stop();
        if let Some(handle) = self.runtime_join.take() {
            // The platform callback may still be finishing a bounded rebuild. Detach here
            // so dropping a CaptureLoop on a Tokio/main thread cannot block that thread;
            // stop() has already queued shutdown and join() remains available explicitly.
            let _ = handle;
        }
        if let Some(handle) = self.join.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{FakeCapturer, FakeEncoder};
    use std::sync::atomic::{AtomicU64, AtomicUsize};

    #[test]
    fn external_frames_share_keyframe_and_stop_state() {
        let (mut capture, input) = CaptureLoop::external(2);
        assert!(input.push(EncodedFrame {
            data: vec![1, 2, 3],
            keyframe: false,
            pts_us: 7,
        }));
        capture.request_keyframe();
        assert!(input.take_keyframe_request());
        assert!(!input.take_keyframe_request());
        capture.stop();
        assert!(capture.is_stopping());
        assert!(!input.push(EncodedFrame {
            data: vec![4],
            keyframe: true,
            pts_us: 8,
        }));
        let err = capture
            .apply_runtime_settings(RuntimeSettings::new(60, 4000))
            .unwrap_err()
            .to_string();
        assert!(err.contains("采集线程不可用"));
        let frame = capture.frames.try_recv().expect("已提交的帧仍在队列中");
        assert_eq!(frame.data, vec![1, 2, 3]);
        assert_eq!(frame.pts_us, 7);
        let stats = capture.stats();
        assert_eq!(stats.frames_yielded, 1);
        assert_eq!(stats.bytes, 3);
        assert_eq!(stats.keyframes, 0);
    }

    #[test]
    fn external_frames_are_dropped_while_no_session_is_active() {
        let (capture, input) = CaptureLoop::external(2);
        capture.pause_when_idle.store(true, Ordering::SeqCst);
        assert!(!input.push(EncodedFrame {
            data: vec![1],
            keyframe: true,
            pts_us: 1,
        }));
        let activity =
            CaptureActivityGuard::acquire_if_running(&capture.active_sessions, &capture.activity)
                .unwrap();
        assert!(input.push(EncodedFrame {
            data: vec![2],
            keyframe: true,
            pts_us: 2,
        }));
        drop(activity);
        assert!(!input.push(EncodedFrame {
            data: vec![3],
            keyframe: true,
            pts_us: 3,
        }));
        assert_eq!(capture.stats().frames_yielded, 1);
        capture.stop();
    }

    #[test]
    fn stopped_capture_refuses_new_activity_without_notifying() {
        let (capture, _input) = CaptureLoop::external(1);
        let events = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&events);
        capture.set_capture_activity_observer(move |active| {
            observed.lock().unwrap().push(active);
        });
        capture.stop();

        assert!(CaptureActivityGuard::acquire_if_running(
            &capture.active_sessions,
            &capture.activity
        )
        .is_none());
        assert!(CaptureActivityGuard::acquire_pending_if_running(&capture.activity).is_none());
        assert_eq!(capture.active_sessions.load(Ordering::SeqCst), 0);
        assert!(events.lock().unwrap().is_empty());
    }

    #[test]
    fn late_observer_registration_does_not_announce_stopped_activity() {
        let (capture, _input) = CaptureLoop::external(1);
        let pending = CaptureActivityGuard::acquire_pending_if_running(&capture.activity).unwrap();
        capture.stop();
        let events = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&events);
        capture.set_capture_activity_observer(move |active| {
            observed.lock().unwrap().push(active);
        });
        assert!(events.lock().unwrap().is_empty());
        drop(pending);
        assert_eq!(*events.lock().unwrap(), vec![false]);
    }

    #[test]
    fn pending_activity_cannot_activate_after_stop() {
        let (capture, _input) = CaptureLoop::external(1);
        let events = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&events);
        capture.set_capture_activity_observer(move |active| {
            observed.lock().unwrap().push(active);
        });
        let mut pending =
            CaptureActivityGuard::acquire_pending_if_running(&capture.activity).unwrap();
        capture.stop();
        assert!(!pending.activate());
        assert_eq!(capture.active_sessions.load(Ordering::SeqCst), 0);
        drop(pending);
        assert_eq!(*events.lock().unwrap(), vec![true, false]);
    }

    #[test]
    fn pending_activation_can_be_rolled_back_without_idle_transition() {
        let (capture, _input) = CaptureLoop::external(1);
        let events = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&events);
        capture.set_capture_activity_observer(move |active| {
            observed.lock().unwrap().push(active);
        });
        let mut pending =
            CaptureActivityGuard::acquire_pending_if_running(&capture.activity).unwrap();
        assert!(pending.activate());
        assert_eq!(capture.active_sessions.load(Ordering::SeqCst), 1);
        pending.deactivate();
        assert_eq!(capture.active_sessions.load(Ordering::SeqCst), 0);
        assert_eq!(*events.lock().unwrap(), vec![true]);
        drop(pending);
        assert_eq!(*events.lock().unwrap(), vec![true, false]);
    }

    #[test]
    fn capture_activity_observer_reports_aggregate_boundaries() {
        let (capture, _input) = CaptureLoop::external(1);
        let events = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&events);
        capture.set_capture_activity_observer(move |active| {
            observed.lock().unwrap().push(active);
        });

        let first =
            CaptureActivityGuard::acquire_if_running(&capture.active_sessions, &capture.activity)
                .unwrap();
        let second =
            CaptureActivityGuard::acquire_if_running(&capture.active_sessions, &capture.activity)
                .unwrap();
        assert_eq!(*events.lock().unwrap(), vec![true]);
        drop(first);
        assert_eq!(*events.lock().unwrap(), vec![true]);
        drop(second);
        assert_eq!(*events.lock().unwrap(), vec![true, false]);

        let mut pending =
            CaptureActivityGuard::acquire_pending_if_running(&capture.activity).unwrap();
        assert_eq!(capture.active_sessions.load(Ordering::SeqCst), 0);
        assert_eq!(*events.lock().unwrap(), vec![true, false, true]);
        assert!(pending.activate());
        assert_eq!(capture.active_sessions.load(Ordering::SeqCst), 1);
        assert_eq!(*events.lock().unwrap(), vec![true, false, true]);
        drop(pending);
        assert_eq!(*events.lock().unwrap(), vec![true, false, true, false]);
        capture.stop();
    }

    #[test]
    fn concurrent_capture_activity_cannot_end_with_a_stale_idle_notice() {
        let (capture, _input) = CaptureLoop::external(1);
        let events = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&events);
        capture.set_capture_activity_observer(move |active| {
            observed.lock().unwrap().push(active);
        });
        let mut threads = Vec::new();
        for _ in 0..4 {
            let active_sessions = Arc::clone(&capture.active_sessions);
            let activity = Arc::clone(&capture.activity);
            threads.push(thread::spawn(move || {
                for _ in 0..100 {
                    let guard =
                        CaptureActivityGuard::acquire_if_running(&active_sessions, &activity)
                            .unwrap();
                    drop(guard);
                }
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(capture.active_sessions.load(Ordering::SeqCst), 0);
        assert_eq!(events.lock().unwrap().last(), Some(&false));
        capture.stop();
    }

    #[test]
    fn external_runtime_applies_and_publishes_only_after_success() {
        let initial = RuntimeSettings {
            fps: 30,
            bitrate_kbps: 4000,
            resolution: Some(EncodeResolution {
                width: 1920,
                height: 1080,
            }),
        };
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let calls = Arc::new(AtomicUsize::new(0));
        let apply_calls = Arc::clone(&calls);
        let (capture, _input) = CaptureLoop::external_with_runtime(0, initial, move |requested| {
            apply_calls.fetch_add(1, Ordering::SeqCst);
            entered_tx.send(requested).unwrap();
            release_rx.recv().unwrap();
            Ok(RuntimeSettings {
                fps: requested.fps,
                bitrate_kbps: requested.bitrate_kbps,
                resolution: Some(EncodeResolution {
                    width: 1280,
                    height: 720,
                }),
            })
        })
        .unwrap();
        let requested = RuntimeSettings::new(60, 5000);
        let (reply, response) = mpsc::channel();
        capture
            .runtime
            .commands
            .send(RuntimeCommand::Apply {
                settings: requested,
                reply,
                cancelled: Arc::new(Mutex::new(false)),
            })
            .unwrap();
        assert_eq!(
            entered_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            requested
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(capture.runtime_control().current(), initial);
        release_tx.send(()).unwrap();
        let applied = response
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        assert_eq!(
            applied.resolution.unwrap(),
            EncodeResolution {
                width: 1280,
                height: 720
            }
        );
        assert_eq!(capture.runtime_control().current(), applied);
        capture.stop();
    }

    #[test]
    fn external_runtime_preserves_native_source_across_degradation() {
        let initial = RuntimeSettings {
            fps: 120,
            bitrate_kbps: 12_000,
            resolution: Some(EncodeResolution {
                width: 1920,
                height: 1080,
            }),
        };
        let native = EncodeResolution {
            width: 3840,
            height: 2160,
        };
        let (capture, _input) =
            CaptureLoop::external_with_runtime_and_source(1, initial, Some(native), Ok).unwrap();
        let degraded = RuntimeSettings {
            fps: 30,
            bitrate_kbps: 1000,
            resolution: Some(EncodeResolution {
                width: 960,
                height: 540,
            }),
        };
        assert_eq!(capture.apply_runtime_settings(degraded).unwrap(), degraded);
        assert_eq!(capture.runtime_control().current(), degraded);
        assert_eq!(capture.runtime_control().source_resolution(), Some(native));
        capture.stop();
    }

    #[test]
    fn external_runtime_errors_preserve_settings_and_channel() {
        let initial = RuntimeSettings::new(30, 4000);
        let calls = Arc::new(AtomicUsize::new(0));
        let apply_calls = Arc::clone(&calls);
        let (capture, _input) = CaptureLoop::external_with_runtime(2, initial, move |_| {
            let call = apply_calls.fetch_add(1, Ordering::SeqCst);
            match call {
                0 => anyhow::bail!("injected apply failure"),
                1 => Ok(RuntimeSettings::new(0, 4000)),
                2 => panic!("injected callback panic"),
                _ => Ok(RuntimeSettings::new(60, 6000)),
            }
        })
        .unwrap();
        for expected in ["injected apply failure", "无效", "panic"] {
            let error = capture
                .apply_runtime_settings(RuntimeSettings::new(60, 6000))
                .unwrap_err()
                .to_string();
            assert!(error.contains(expected), "unexpected error: {error}");
            assert_eq!(capture.runtime_control().current(), initial);
        }
        assert_eq!(
            capture
                .apply_runtime_settings(RuntimeSettings::new(60, 6000))
                .unwrap(),
            RuntimeSettings::new(60, 6000)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        capture.stop();
    }

    #[test]
    fn external_runtime_cancelled_command_does_not_apply() {
        let initial = RuntimeSettings::new(30, 4000);
        let calls = Arc::new(AtomicUsize::new(0));
        let apply_calls = Arc::clone(&calls);
        let (capture, _input) = CaptureLoop::external_with_runtime(2, initial, move |settings| {
            apply_calls.fetch_add(1, Ordering::SeqCst);
            Ok(settings)
        })
        .unwrap();
        let (reply, response) = mpsc::channel();
        capture
            .runtime
            .commands
            .send(RuntimeCommand::Apply {
                settings: RuntimeSettings::new(60, 6000),
                reply,
                cancelled: Arc::new(Mutex::new(true)),
            })
            .unwrap();
        let error = response
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("已取消"));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(capture.runtime_control().current(), initial);
        assert!(capture.apply_runtime_settings(initial).is_ok());
        capture.stop();
    }

    #[test]
    fn external_runtime_cancellation_during_apply_does_not_publish() {
        let initial = RuntimeSettings::new(30, 4000);
        let cancelled = Arc::new(Mutex::new(false));
        let apply_cancelled = Arc::clone(&cancelled);
        let (capture, _input) = CaptureLoop::external_with_runtime(1, initial, move |_| {
            *apply_cancelled
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
            Ok(RuntimeSettings::new(60, 6000))
        })
        .unwrap();
        let (reply, response) = mpsc::channel();
        capture
            .runtime
            .commands
            .send(RuntimeCommand::Apply {
                settings: RuntimeSettings::new(60, 6000),
                reply,
                cancelled,
            })
            .unwrap();
        let error = response
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("已取消"));
        assert_eq!(capture.runtime_control().current(), initial);
        capture.stop();
    }

    #[test]
    fn external_runtime_timeout_wins_before_publication() {
        let initial = RuntimeSettings::new(30, 4000);
        let cancelled = Arc::new(Mutex::new(false));
        let (control, _replies) = CaptureRuntimeControl::new(initial);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker_cancelled = Arc::clone(&cancelled);
        let worker_control = control.clone();
        let worker = thread::spawn(move || {
            let mut current = initial;
            apply_external_settings(
                RuntimeSettings::new(60, 6000),
                &worker_cancelled,
                &move |_| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                    Ok(RuntimeSettings::new(60, 6000))
                },
                &mut current,
                &worker_control,
            )
        });
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let mut cancellation = cancelled
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *cancellation = true;
        release_tx.send(()).unwrap();
        drop(cancellation);
        let error = worker.join().unwrap().unwrap_err();
        assert!(error.to_string().contains("已取消"));
        assert_eq!(control.current(), initial);
    }

    #[test]
    fn external_runtime_validates_initial_and_all_bounds() {
        for invalid in [
            RuntimeSettings::new(0, 4000),
            RuntimeSettings::new(121, 4000),
            RuntimeSettings::new(30, 99),
            RuntimeSettings::new(30, 100_001),
            RuntimeSettings {
                fps: 30,
                bitrate_kbps: 4000,
                resolution: Some(EncodeResolution {
                    width: 0,
                    height: 2,
                }),
            },
            RuntimeSettings {
                fps: 30,
                bitrate_kbps: 4000,
                resolution: Some(EncodeResolution {
                    width: 2,
                    height: 3,
                }),
            },
        ] {
            assert!(CaptureLoop::external_with_runtime(1, invalid, Ok).is_err());
        }
        let minimum = RuntimeSettings {
            fps: 1,
            bitrate_kbps: 100,
            resolution: Some(EncodeResolution {
                width: 2,
                height: 2,
            }),
        };
        let maximum = RuntimeSettings {
            fps: 120,
            bitrate_kbps: 100_000,
            resolution: Some(EncodeResolution {
                width: 3840,
                height: 2160,
            }),
        };
        let (capture, _input) = CaptureLoop::external_with_runtime(1, minimum, Ok).unwrap();
        assert_eq!(capture.apply_runtime_settings(maximum).unwrap(), maximum);
        assert_eq!(capture.runtime_control().current(), maximum);
        assert!(capture
            .apply_runtime_settings(RuntimeSettings::new(121, 4000))
            .is_err());
        assert_eq!(capture.runtime_control().current(), maximum);
        capture.stop();
    }

    #[test]
    fn external_runtime_rejects_invalid_applied_dimensions() {
        let initial = RuntimeSettings::new(30, 4000);
        let (capture, _input) = CaptureLoop::external_with_runtime(1, initial, |_| {
            Ok(RuntimeSettings {
                fps: 30,
                bitrate_kbps: 4000,
                resolution: Some(EncodeResolution {
                    width: 1921,
                    height: 1080,
                }),
            })
        })
        .unwrap();
        let error = capture.apply_runtime_settings(initial).unwrap_err();
        assert!(format!("{error:#}").contains("编码分辨率"), "{error:#}");
        assert_eq!(capture.runtime_control().current(), initial);
        capture.stop();
    }

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

    #[test]
    fn retained_capture_pauses_without_an_active_session() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (release_tx, release_rx) = mpsc::channel();
            let mut capture = start_capture_loop(
                CaptureLoopConfig {
                    capture_timeout_ms: 1,
                    max_fps: 30,
                    queue_capacity: 2,
                    max_consecutive_errors: 3,
                },
                move || {
                    release_rx.recv().unwrap();
                    Ok((FakeCapturer::new(64, 48, u64::MAX), FakeEncoder::new()))
                },
            )
            .unwrap();
            capture.pause_when_idle.store(true, Ordering::SeqCst);
            release_tx.send(()).unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_eq!(capture.stats().captured, 0);

            let activity = CaptureActivityGuard::acquire_if_running(
                &capture.active_sessions,
                &capture.activity,
            )
            .unwrap();
            assert!(
                tokio::time::timeout(Duration::from_secs(2), capture.frames.recv())
                    .await
                    .unwrap()
                    .is_some()
            );
            drop(activity);
            tokio::time::sleep(Duration::from_millis(80)).await;
            let paused_at = capture.stats().captured;
            tokio::time::sleep(Duration::from_millis(120)).await;
            assert_eq!(capture.stats().captured, paused_at);
            capture.stop();
            capture.join().unwrap().unwrap();
        });
    }

    #[test]
    fn capture_recovery_requests_keyframe_on_next_frame() {
        struct RecoveringCapturer {
            inner: FakeCapturer,
            recovered: bool,
            calls: u32,
        }

        impl Capturer for RecoveringCapturer {
            fn capture(&mut self, timeout_ms: u32) -> anyhow::Result<Option<RawFrame>> {
                if self.calls == 2 {
                    self.calls += 1;
                    self.recovered = true;
                    return Ok(None);
                }
                self.calls += 1;
                self.inner.capture(timeout_ms)
            }

            fn take_recovery_request(&mut self) -> bool {
                std::mem::take(&mut self.recovered)
            }
        }

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut capture = start_capture_loop(
                CaptureLoopConfig {
                    max_fps: 120,
                    ..CaptureLoopConfig::default()
                },
                || {
                    Ok((
                        RecoveringCapturer {
                            inner: FakeCapturer::new(64, 48, 3),
                            recovered: false,
                            calls: 0,
                        },
                        FakeEncoder::new(),
                    ))
                },
            )
            .unwrap();
            let initial = tokio::time::timeout(Duration::from_secs(2), capture.frames.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(initial.keyframe);
            let before = tokio::time::timeout(Duration::from_secs(2), capture.frames.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(!before.keyframe);
            let after = tokio::time::timeout(Duration::from_secs(2), capture.frames.recv()).await;
            if after.is_err() {
                capture.stop();
                let stats = capture.join().unwrap().unwrap();
                panic!("恢复后未收到关键帧：{stats:?}");
            }
            assert!(after.unwrap().unwrap().keyframe);
            capture.stop();
            let stats = capture.join().unwrap().unwrap();
            assert_eq!(stats.capture_errors, 0);
        });
    }

    #[test]
    fn resolution_limit_preserves_aspect_and_never_upscales() {
        let fitted = EncodeResolution {
            width: 1280,
            height: 700,
        }
        .fit(1920, 1080)
        .unwrap();
        assert_eq!((fitted.width, fitted.height), (1244, 700));
        let native = EncodeResolution {
            width: 3840,
            height: 2160,
        }
        .fit(1920, 1080)
        .unwrap();
        assert_eq!((native.width, native.height), (1920, 1080));
        assert!(EncodeResolution {
            width: 33,
            height: 18,
        }
        .fit(64, 48)
        .is_err());
    }

    #[test]
    fn scaler_handles_padding_downscale_and_upscale() {
        let mut scaler = FrameScaler::default();
        let frame = RawFrame {
            width: 4,
            height: 4,
            stride: 20,
            data: {
                let mut data = Vec::new();
                for y in 0..4 {
                    for x in 0..4 {
                        data.extend_from_slice(&[(x * 60) as u8, (y * 60) as u8, 90, 255]);
                    }
                    data.extend_from_slice(&[0xEE; 4]);
                }
                data
            },
            pts_us: 42,
            full_frame: true,
        };
        let small = scaler
            .scale(
                &frame,
                EncodeResolution {
                    width: 2,
                    height: 2,
                },
            )
            .unwrap();
        assert_eq!(small.data.len(), 16);
        assert!(!small.data.contains(&0xEE));
        assert_eq!(small.pts_us, 42);
        assert!(small.data.chunks_exact(4).all(|p| p[3] == 255));
        let large = scaler
            .scale(
                &frame,
                EncodeResolution {
                    width: 8,
                    height: 8,
                },
            )
            .unwrap();
        assert_eq!(large.data.len(), 256);
        assert!(large.data[3] == 255);
        assert!(large.data[255] == 255);
    }

    #[derive(Default)]
    struct EncoderEvents {
        settings: Mutex<Vec<RuntimeSettings>>,
        builds: AtomicUsize,
        fail_build: AtomicBool,
        fail_bitrate: AtomicBool,
        frames: AtomicU64,
        keyframes: AtomicU64,
    }

    struct TestEncoder {
        settings: RuntimeSettings,
        events: Arc<EncoderEvents>,
        pending_keyframe: bool,
    }

    impl Encoder for TestEncoder {
        fn encode(&mut self, frame: &RawFrame) -> anyhow::Result<Vec<EncodedFrame>> {
            self.events.frames.fetch_add(1, Ordering::SeqCst);
            if self.pending_keyframe {
                self.events.keyframes.fetch_add(1, Ordering::SeqCst);
            }
            let mut data = Vec::new();
            data.extend_from_slice(&frame.width.to_be_bytes());
            data.extend_from_slice(&frame.height.to_be_bytes());
            data.extend_from_slice(&self.settings.fps.to_be_bytes());
            data.extend_from_slice(&self.settings.bitrate_kbps.to_be_bytes());
            let keyframe = self.pending_keyframe;
            self.pending_keyframe = false;
            Ok(vec![EncodedFrame {
                data,
                keyframe,
                pts_us: frame.pts_us,
            }])
        }

        fn request_keyframe(&mut self) {
            self.pending_keyframe = true;
        }

        fn set_bitrate(&mut self, kbps: u32) -> anyhow::Result<()> {
            if self.events.fail_bitrate.load(Ordering::SeqCst) {
                anyhow::bail!("injected bitrate failure");
            }
            self.settings.bitrate_kbps = kbps;
            self.events.settings.lock().unwrap().push(self.settings);
            Ok(())
        }

        fn set_frame_rate(&mut self, fps: u32) -> anyhow::Result<()> {
            self.settings.fps = fps;
            self.events.settings.lock().unwrap().push(self.settings);
            Ok(())
        }

        fn runtime_settings(&self) -> Option<RuntimeSettings> {
            Some(self.settings)
        }

        fn flush(&mut self) -> anyhow::Result<Vec<EncodedFrame>> {
            Ok(Vec::new())
        }
    }

    fn start_test_loop(events: Arc<EncoderEvents>) -> anyhow::Result<CaptureLoop> {
        let initial = RuntimeSettings::new(60, 4000);
        let build_events = Arc::clone(&events);
        start_capture_loop_with_encoder_factory(
            CaptureLoopConfig {
                capture_timeout_ms: 1,
                max_fps: 60,
                queue_capacity: 32,
                max_consecutive_errors: 3,
            },
            initial,
            move || {
                Ok((
                    FakeCapturer::new(64, 48, u64::MAX),
                    TestEncoder {
                        settings: initial,
                        events: Arc::clone(&build_events),
                        pending_keyframe: false,
                    },
                ))
            },
            move |settings| {
                events.builds.fetch_add(1, Ordering::SeqCst);
                if events.fail_build.load(Ordering::SeqCst) {
                    anyhow::bail!("injected encoder build failure");
                }
                Ok(TestEncoder {
                    settings: *settings,
                    events: Arc::clone(&events),
                    pending_keyframe: false,
                })
            },
        )
    }

    fn read_u32(data: &[u8], offset: usize) -> u32 {
        u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap())
    }

    #[test]
    fn runtime_control_rebuilds_scales_and_requests_keyframe() {
        let events = Arc::new(EncoderEvents::default());
        let mut capture = start_test_loop(Arc::clone(&events)).unwrap();
        let applied = capture
            .apply_runtime_settings(RuntimeSettings {
                fps: 120,
                bitrate_kbps: 3000,
                resolution: Some(EncodeResolution {
                    width: 32,
                    height: 24,
                }),
            })
            .unwrap();
        assert_eq!(applied.resolution.unwrap().width, 32);
        assert_eq!(capture.runtime_control().current(), applied);

        let frame = capture.frames.blocking_recv().unwrap();
        assert_eq!(read_u32(&frame.data, 0), 32);
        assert_eq!(read_u32(&frame.data, 4), 24);
        assert_eq!(read_u32(&frame.data, 8), 120);
        assert_eq!(read_u32(&frame.data, 12), 3000);
        assert!(frame.keyframe, "新编码器首帧必须是关键帧");
        assert_eq!(events.builds.load(Ordering::SeqCst), 1);
        assert_eq!(events.keyframes.load(Ordering::SeqCst), 1);

        capture.stop();
        let stats = capture.join().unwrap().unwrap();
        assert_eq!(stats.encoder_rebuilds, 1);
        assert_eq!(stats.settings_applied, 1);
        assert_eq!(stats.settings_errors, 0);
    }

    #[test]
    fn failed_rebuild_preserves_old_encoder_and_reports_error() {
        let events = Arc::new(EncoderEvents::default());
        let mut capture = start_test_loop(Arc::clone(&events)).unwrap();
        events.fail_build.store(true, Ordering::SeqCst);
        let err = capture
            .apply_runtime_settings(RuntimeSettings {
                fps: 120,
                bitrate_kbps: 3000,
                resolution: Some(EncodeResolution {
                    width: 32,
                    height: 24,
                }),
            })
            .unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("重建失败") && message.contains("injected"),
            "unexpected rebuild error: {message}"
        );
        assert_eq!(capture.runtime_control().current().fps, 60);
        let frame = capture.frames.blocking_recv().unwrap();
        assert_eq!(read_u32(&frame.data, 0), 64);
        assert_eq!(read_u32(&frame.data, 4), 48);
        assert_eq!(read_u32(&frame.data, 8), 60);
        capture.stop();
        let stats = capture.join().unwrap().unwrap();
        assert_eq!(stats.encoder_rebuilds, 0);
        assert_eq!(stats.settings_errors, 1);
        assert!(stats.last_settings_error.is_some());
    }

    #[test]
    fn runtime_control_is_cross_thread_and_rejects_invalid_values() {
        let events = Arc::new(EncoderEvents::default());
        let mut capture = start_test_loop(Arc::clone(&events)).unwrap();
        let control = capture.runtime_control();
        let worker = control.clone();
        let handle = thread::spawn(move || worker.apply(RuntimeSettings::new(120, 5000)));
        handle.join().unwrap().unwrap();
        let current = control.current();
        assert_eq!(current.fps, 120);
        assert_eq!(current.bitrate_kbps, 5000);
        assert!(control.apply(RuntimeSettings::new(121, 5000)).is_err());
        assert_eq!(control.current(), current);
        assert_eq!(events.builds.load(Ordering::SeqCst), 0);
        capture.stop();
        let stats = capture.join().unwrap().unwrap();
        assert_eq!(stats.settings_applied, 1);
        assert!(stats.encoded > 0);
    }

    #[test]
    fn legacy_loop_returns_precise_resolution_rebuild_error() {
        let mut capture = start_capture_loop(
            CaptureLoopConfig {
                capture_timeout_ms: 1,
                ..CaptureLoopConfig::default()
            },
            || Ok((FakeCapturer::new(64, 48, u64::MAX), FakeEncoder::new())),
        )
        .unwrap();
        let err = capture
            .apply_runtime_settings(RuntimeSettings {
                fps: 60,
                bitrate_kbps: 4000,
                resolution: Some(EncodeResolution {
                    width: 32,
                    height: 24,
                }),
            })
            .unwrap_err()
            .to_string();
        assert!(err.contains("没有编码器工厂") && err.contains("安全重建"));
        capture.stop();
        let _ = capture.join();
    }

    #[test]
    fn bitrate_failure_rolls_back_fps_and_preserves_reported_settings() {
        let events = Arc::new(EncoderEvents::default());
        let build_events = Arc::clone(&events);
        let initial = RuntimeSettings::new(60, 4000);
        let mut capture = start_capture_loop(
            CaptureLoopConfig {
                capture_timeout_ms: 1,
                max_fps: 60,
                queue_capacity: 32,
                max_consecutive_errors: 3,
            },
            move || {
                Ok((
                    FakeCapturer::new(64, 48, u64::MAX),
                    TestEncoder {
                        settings: initial,
                        events: Arc::clone(&build_events),
                        pending_keyframe: false,
                    },
                ))
            },
        )
        .unwrap();
        events.fail_bitrate.store(true, Ordering::SeqCst);
        let err = capture
            .apply_runtime_settings(RuntimeSettings::new(120, 5000))
            .unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("动态码率调整失败") && message.contains("injected"));
        assert_eq!(
            capture.runtime_control().current(),
            RuntimeSettings::new(60, 4000)
        );
        let settings = events.settings.lock().unwrap();
        assert_eq!(settings.last(), Some(&RuntimeSettings::new(60, 4000)));
        drop(settings);

        let frame = capture.frames.blocking_recv().unwrap();
        assert_eq!(read_u32(&frame.data, 8), 60);
        assert_eq!(read_u32(&frame.data, 12), 4000);
        capture.stop();
        let stats = capture.join().unwrap().unwrap();
        assert_eq!(stats.settings_applied, 0);
        assert_eq!(stats.settings_errors, 1);
    }

    #[test]
    fn unsupported_dynamic_control_falls_back_to_keyframed_rebuild() {
        let events = Arc::new(EncoderEvents::default());
        let mut capture = start_test_loop(Arc::clone(&events)).unwrap();
        events.fail_bitrate.store(true, Ordering::SeqCst);
        let applied = capture
            .apply_runtime_settings(RuntimeSettings::new(120, 5000))
            .unwrap();
        assert_eq!(applied.fps, 120);
        assert_eq!(applied.bitrate_kbps, 5000);
        assert_eq!(
            applied.resolution,
            Some(EncodeResolution {
                width: 64,
                height: 48,
            })
        );
        assert_eq!(events.builds.load(Ordering::SeqCst), 1);

        let frame = capture.frames.blocking_recv().unwrap();
        assert_eq!(read_u32(&frame.data, 0), 64);
        assert_eq!(read_u32(&frame.data, 4), 48);
        assert_eq!(read_u32(&frame.data, 8), 120);
        assert_eq!(read_u32(&frame.data, 12), 5000);
        assert!(frame.keyframe);
        capture.stop();
        let stats = capture.join().unwrap().unwrap();
        assert_eq!(stats.encoder_rebuilds, 1);
        assert_eq!(stats.settings_applied, 1);
        assert_eq!(stats.settings_errors, 0);
    }

    #[test]
    fn explicit_keyframe_request_reaches_capture_thread_encoder() {
        let events = Arc::new(EncoderEvents::default());
        let mut capture = start_test_loop(Arc::clone(&events)).unwrap();
        capture.request_keyframe();
        let mut saw_keyframe = false;
        for _ in 0..20 {
            if capture.frames.blocking_recv().unwrap().keyframe {
                saw_keyframe = true;
                break;
            }
        }
        assert!(saw_keyframe, "CaptureLoop::request_keyframe 未传到编码器");
        assert!(events.keyframes.load(Ordering::SeqCst) >= 1);
        capture.stop();
        let _ = capture.join().unwrap();
    }

    struct UnknownDimensionsCapturer(FakeCapturer);

    impl Capturer for UnknownDimensionsCapturer {
        fn capture(&mut self, timeout_ms: u32) -> anyhow::Result<Option<RawFrame>> {
            self.0.capture(timeout_ms)
        }

        fn dimensions(&self) -> Option<(u32, u32)> {
            None
        }
    }

    #[test]
    fn unknown_source_dimensions_keep_scaled_encoder_and_explicit_initial_fps() {
        let events = Arc::new(EncoderEvents::default());
        let build_events = Arc::clone(&events);
        let rebuild_events = Arc::clone(&events);
        let initial = RuntimeSettings {
            fps: 120,
            bitrate_kbps: 3000,
            resolution: Some(EncodeResolution {
                width: 32,
                height: 24,
            }),
        };
        let mut capture = start_capture_loop_with_encoder_factory(
            CaptureLoopConfig {
                capture_timeout_ms: 1,
                max_fps: 60,
                ..CaptureLoopConfig::default()
            },
            initial,
            move || {
                Ok((
                    UnknownDimensionsCapturer(FakeCapturer::new(64, 48, u64::MAX)),
                    TestEncoder {
                        settings: initial,
                        events: Arc::clone(&build_events),
                        pending_keyframe: false,
                    },
                ))
            },
            move |settings| {
                rebuild_events.builds.fetch_add(1, Ordering::SeqCst);
                Ok(TestEncoder {
                    settings: *settings,
                    events: Arc::clone(&rebuild_events),
                    pending_keyframe: false,
                })
            },
        )
        .unwrap();
        let frame = capture.frames.blocking_recv().unwrap();
        assert_eq!(read_u32(&frame.data, 0), 32);
        assert_eq!(read_u32(&frame.data, 4), 24);
        assert_eq!(read_u32(&frame.data, 8), 120);
        assert_eq!(events.builds.load(Ordering::SeqCst), 0);
        capture.stop();
        let _ = capture.join().unwrap();
    }

    struct UnflushableEncoder;

    impl Encoder for UnflushableEncoder {
        fn encode(&mut self, _frame: &RawFrame) -> anyhow::Result<Vec<EncodedFrame>> {
            anyhow::bail!("test encoder must not encode")
        }

        fn request_keyframe(&mut self) {}
    }

    #[test]
    fn unimplemented_flush_is_an_explicit_error_not_placeholder_success() {
        let err = UnflushableEncoder.flush().unwrap_err().to_string();
        assert!(err.contains("未实现 flush") && err.contains("不能安全重建"));
    }

    #[test]
    fn factory_start_rejects_invalid_legacy_fps_setting() {
        let err = start_capture_loop_with_encoder_factory(
            CaptureLoopConfig {
                max_fps: 121,
                ..CaptureLoopConfig::default()
            },
            RuntimeSettings::new(60, 4000),
            || Ok((FakeCapturer::new(64, 48, 1), FakeEncoder::new())),
            |_| Ok(FakeEncoder::new()),
        )
        .err()
        .expect("invalid max_fps must be rejected")
        .to_string();
        assert!(err.contains("max_fps") && err.contains("121"));
    }
}
