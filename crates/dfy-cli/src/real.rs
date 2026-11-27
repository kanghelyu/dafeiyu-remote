// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 真实链路：屏幕采集 + Media Foundation H.264 编码 → QUIC → 解码。
//!
//! 这是 MVP 的「能看见画面」验证路径：`dfy agent`（被控端）与 `dfy view`（主控端）。
//! 目前只传视频 DATAGRAM + 心跳；输入注入/剪贴板/文件会在此之上继续接。

use std::net::SocketAddr;
use std::path::PathBuf;
#[cfg(any(windows, target_os = "linux"))]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(any(windows, target_os = "linux"))]
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use dfy_path::{Action, PathConfig, PathKind, PathMonitor, PathSample};
#[cfg(any(windows, target_os = "linux"))]
use dfy_proto::consts::PERM_AUDIO_OUT;
use dfy_proto::consts::{
    CAPABILITY_QUALITY_CONTROL, INPUT_KIND_KEY, INPUT_KIND_MOUSE_BTN, INPUT_KIND_MOUSE_MOVE,
    INPUT_KIND_WHEEL, INPUT_STATE_DOWN, INPUT_STATE_UP, PERM_CLIPBOARD, PERM_CONTROL, PERM_FILE,
    PERM_VIEW,
};
use dfy_proto::envelope;
use dfy_proto::msg::InputEvent as ProtoInputEvent;
use dfy_relay::Role;
#[cfg(target_os = "linux")]
use dfy_session::Capturer;
#[cfg(any(windows, target_os = "linux"))]
use dfy_session::{
    serve_agent_shared, start_capture_loop_with_encoder_factory, AgentConfig, CaptureHandle,
    CaptureLoop, CaptureLoopConfig, MigrationContext, RuntimeSettings, SessionCoordinator,
};
use dfy_session::{
    Decoder as _, FrameAssembler, RawFrame, StreamFeedbackWindow, FEEDBACK_INTERVAL,
};
#[cfg(any(windows, target_os = "linux"))]
use dfy_transport::{
    agent_endpoint_on_socket, bind_agent, bind_agent_with_identity, ServerAuth, TlsIdentity,
};
use dfy_transport::{
    client_handshake, client_resume_handshake, client_tls, connect, connect_on_socket,
    parse_fingerprint_hex, send_input_move, ClientSession,
};
use dfy_ui::{spawn_viewer, ViewerConfig, ViewerEvent};
use ed25519_dalek::SigningKey;

use crate::relay_bridge::RelayOptions;
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::adapters::MftDecoderAdapter;
#[cfg(any(windows, target_os = "linux"))]
use crate::adapters::MftEncoderAdapter;
#[cfg(any(windows, target_os = "linux"))]
use crate::clipboard_bridge::AgentHandler;
use crate::clipboard_bridge::ViewClipboard;
use crate::credentials::SessionPassword;
use crate::file_bridge::{is_file_envelope, ViewFiles};
use crate::input_capture::{CtrlMsg, InputCapture};
pub use crate::p2p_bridge::P2POptions;

#[cfg(target_os = "linux")]
struct CliCapturer {
    inner: Box<dyn Capturer + Send>,
}

#[cfg(target_os = "linux")]
impl Capturer for CliCapturer {
    fn capture(&mut self, timeout_ms: u32) -> Result<Option<RawFrame>> {
        self.inner.capture(timeout_ms)
    }

    fn dimensions(&self) -> Option<(u32, u32)> {
        self.inner.dimensions()
    }

    fn take_recovery_request(&mut self) -> bool {
        self.inner.take_recovery_request()
    }
}

#[cfg(target_os = "linux")]
struct RecoveringVirtualCapturer {
    endpoint: String,
    live_endpoint: Arc<Mutex<String>>,
    displays: Arc<crate::virtual_display::ManagedVirtualDisplays>,
    capturer: dfy_capture::LinuxCapturer,
    recovery_requested: bool,
}

#[cfg(target_os = "linux")]
impl Capturer for RecoveringVirtualCapturer {
    fn capture(&mut self, timeout_ms: u32) -> Result<Option<RawFrame>> {
        match self.capturer.capture(timeout_ms) {
            Err(err) => {
                warn!(error = %err, "虚拟显示采集失败，正在恢复并重连");
                self.endpoint = self.displays.capture_endpoint()?;
                *self
                    .live_endpoint
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = self.endpoint.clone();
                self.capturer = dfy_capture::LinuxCapturer::on_display(&self.endpoint, 0)?;
                self.recovery_requested = true;
                Ok(None)
            }
            Ok(frame) => Ok(frame.map(|frame| RawFrame {
                width: frame.width,
                height: frame.height,
                stride: frame.stride,
                data: frame.data,
                pts_us: dfy_transport::now_us(),
                full_frame: frame.full_frame,
            })),
        }
    }

    fn dimensions(&self) -> Option<(u32, u32)> {
        self.capturer.dimensions()
    }

    fn take_recovery_request(&mut self) -> bool {
        std::mem::take(&mut self.recovery_requested)
    }
}

/// 被控端（Agent）参数。
#[derive(Debug, Clone)]
pub struct AgentOptions {
    /// 监听地址（UDP/QUIC）。
    pub addr: SocketAddr,
    /// 连接口令（MVP: HMAC-SHA256(SHA256(pw), nonce)）。
    pub password: SessionPassword,
    /// 采集哪个显示器（索引来自 DXGI 枚举顺序）。
    pub monitor: u32,
    /// 进程生命周期内创建的独立虚拟显示数量；0 表示物理桌面。
    pub virtual_displays: u32,
    /// 选择采集的虚拟显示索引。
    pub virtual_monitor: u32,
    /// 虚拟显示几何与元数据。
    pub virtual_spec: dfy_vdisplay::VirtualDisplaySpec,
    /// 目标帧率。
    pub fps: u32,
    /// 目标码率（kbps）。
    pub bitrate_kbps: u32,
    /// 送满多少帧后结束一个会话（`None` = 一直跑）。
    pub max_frames: Option<u64>,
    /// 是否把控制端发来的输入注入本机（`--allow-input` 开启）。
    pub allow_input: bool,
    /// 是否与客户端双向同步剪贴板（`--allow-clipboard` 开启）。
    pub allow_clipboard: bool,
    /// 是否采集本机音频并以音频 DATAGRAM 发给主控（`--audio-out`）。
    pub audio_out: bool,
    /// 诊断：用合成正弦波代替麦克风（需 --audio-out；便于无输入设备环境验收链路）。
    pub audio_test_tone: bool,
    /// 接收控制端文件的目录（`None` = 不接收）。
    pub recv_dir: Option<std::path::PathBuf>,
    /// 经中继接入（`None` = 直连/公网打洞）。
    pub relay: Option<RelayOptions>,
    /// 同时监听直连与中继端点，接受可恢复会话迁移。
    pub auto_path: bool,
    /// 诊断用：View 强制迁移一次；Agent 保留该字段以共享 CLI 参数形状。
    #[allow(dead_code)]
    pub force_switch_once: Option<PathKind>,
    /// 诊断/部署用：--auto-path 的起始路径，默认 direct。
    #[allow(dead_code)]
    pub auto_initial_path: Option<PathKind>,
    /// P2P 直连打洞（`None` = 不做打洞，直接 bind/connect）。
    pub p2p: Option<P2POptions>,
    /// 视频编码格式（Windows 默认 H.264；非 Windows 默认 MJPEG 兜底）。
    pub codec: dfy_codec::Codec,
}

/// 主控端（View）参数。
#[derive(Debug, Clone)]
pub struct ViewOptions {
    /// Agent 地址。
    pub addr: SocketAddr,
    /// 连接口令。
    pub password: SessionPassword,
    /// 严格校验 Agent 证书的 SPKI 指纹。
    pub expect_fingerprint: Option<[u8; 32]>,
    /// 观察多久（秒）。
    pub seconds: u64,
    /// 保存第一帧解码结果为裸 BGRA（便于人工核对）。
    pub save_first_frame: Option<PathBuf>,
    /// 是否采集本地鼠标/键盘并发给 Agent（`--control` 开启；Windows/Linux X11）。
    pub control: bool,
    /// 打开图形窗口显示远端画面（`--ui`；Windows/Linux 需要图形环境）。
    pub ui: bool,
    /// UI 窗口启动即全屏（`--fullscreen`，只在 `--ui` 时生效）。
    pub fullscreen: bool,
    /// 是否与 Agent 双向同步剪贴板（`--clipboard` 开启）。
    pub clipboard: bool,
    /// 是否接收 Agent 音频并播放（`--audio-in`）。
    pub audio_in: bool,
    /// 播放音量 0..=100（`--volume`）。
    pub audio_volume: u8,
    /// 静音（`--mute`；仍接收并解码，只是输出 0）。
    pub audio_mute: bool,
    /// 要发送给 Agent 的本地文件/目录（空 = 不发送）。
    pub send_files: Vec<PathBuf>,
    /// 经中继接入（`None` = 直连）。
    pub relay: Option<RelayOptions>,
    /// 质量劣化时自动切换到配置的中继，并尝试可恢复会话迁移。
    pub auto_path: bool,
    /// 诊断用：首个采样周期强制迁移到指定路径一次。
    pub force_switch_once: Option<PathKind>,
    /// --auto-path 的起始路径（direct 或 relay），默认 direct。
    pub auto_initial_path: Option<PathKind>,
    /// P2P 直连打洞（`None` = 直连/中继）。
    pub p2p: Option<P2POptions>,
    /// 视频编码格式（必须与 Agent 一致）。
    pub codec: dfy_codec::Codec,
    /// 请求的视频宽度上限（像素；四个画质参数需一起提供）。
    pub quality_width: Option<u32>,
    /// 请求的视频高度上限（像素；四个画质参数需一起提供）。
    pub quality_height: Option<u32>,
    /// 请求的视频帧率上限（1..=120；四个画质参数需一起提供）。
    pub quality_fps: Option<u32>,
    /// 请求的视频码率上限（100..=100000 kbps；四个画质参数需一起提供）。
    pub quality_bitrate: Option<u32>,
    /// 恢复 Agent 启动时的画质上限。
    pub reset_quality: bool,
    /// 候选路径主动探测包大小（64..=1200 字节，默认 1200）。
    pub probe_bytes: usize,
}

impl ViewOptions {
    /// 本地校验画质选项并构造最多一条 QualityControl；未请求时返回 `None`。
    fn quality_control(&self) -> Result<Option<dfy_proto::Envelope>> {
        quality_control_envelope(
            self.reset_quality,
            self.quality_width,
            self.quality_height,
            self.quality_fps,
            self.quality_bitrate,
        )
    }
}

fn quality_control_envelope(
    reset_quality: bool,
    width: Option<u32>,
    height: Option<u32>,
    fps: Option<u32>,
    bitrate_kbps: Option<u32>,
) -> Result<Option<dfy_proto::Envelope>> {
    let has_request =
        width.is_some() || height.is_some() || fps.is_some() || bitrate_kbps.is_some();
    anyhow::ensure!(
        !reset_quality || !has_request,
        "--reset-quality 不能与 --quality-width/--quality-height/--quality-fps/--quality-bitrate 同时使用"
    );
    if !has_request {
        return Ok(reset_quality.then(|| dfy_proto::Envelope::quality_control(0, 0, 0, 0)));
    }

    let (Some(width), Some(height), Some(fps), Some(bitrate_kbps)) =
        (width, height, fps, bitrate_kbps)
    else {
        anyhow::bail!(
            "画质请求必须同时提供 --quality-width、--quality-height、--quality-fps、--quality-bitrate"
        );
    };
    anyhow::ensure!(
        width > 0 && height > 0,
        "--quality-width/--quality-height 必须大于 0（收到 {width}x{height}）；恢复启动上限请使用 --reset-quality"
    );
    anyhow::ensure!(
        width % 2 == 0 && height % 2 == 0,
        "--quality-width/--quality-height 必须为偶数（收到 {width}x{height}）"
    );
    anyhow::ensure!(
        (1..=120).contains(&fps),
        "--quality-fps 必须在 1..=120 之间（收到 {fps}）"
    );
    anyhow::ensure!(
        (100..=100_000).contains(&bitrate_kbps),
        "--quality-bitrate 必须在 100..=100000 kbps 之间（收到 {bitrate_kbps}）"
    );
    Ok(Some(dfy_proto::Envelope::quality_control(
        width,
        height,
        fps,
        bitrate_kbps,
    )))
}

/// 启动真实 Agent：监听、握手、采集+编码、推流，直到 Ctrl+C。
pub async fn run_agent(opts: AgentOptions) -> Result<()> {
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = &opts;
        anyhow::bail!(
            "当前平台的采集后端尚未实现（v1 支持 Windows WGC/DXGI 与 Linux X11；macOS 待做）"
        );
    }

    #[cfg(any(windows, target_os = "linux"))]
    {
        #[cfg(target_os = "linux")]
        use dfy_capture::LinuxCapturer as PlatformCapturer;
        #[cfg(windows)]
        use dfy_capture::WindowsCapturer as PlatformCapturer;

        let virtual_displays = if opts.virtual_displays == 0 {
            None
        } else {
            let count = opts.virtual_displays;
            let selected = opts.virtual_monitor;
            let spec = opts.virtual_spec.clone();
            Some(Arc::new(
                tokio::task::spawn_blocking(move || {
                    crate::virtual_display::ManagedVirtualDisplays::create(count, selected, &spec)
                })
                .await??,
            ))
        };
        let capture_endpoint = if let Some(displays) = &virtual_displays {
            let displays = Arc::clone(displays);
            Some(tokio::task::spawn_blocking(move || displays.capture_endpoint()).await??)
        } else {
            None
        };
        #[cfg(target_os = "linux")]
        let live_capture_endpoint = capture_endpoint
            .as_ref()
            .map(|endpoint| Arc::new(Mutex::new(endpoint.clone())));

        #[cfg(target_os = "linux")]
        let vdisplay_monitor_stop = Arc::new(AtomicBool::new(false));
        #[cfg(target_os = "linux")]
        let vdisplay_monitor = virtual_displays.as_ref().map(|displays| {
            let displays = Arc::clone(displays);
            let stop = Arc::clone(&vdisplay_monitor_stop);
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(5));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                while !stop.load(Ordering::SeqCst) {
                    tick.tick().await;
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let displays = Arc::clone(&displays);
                    match tokio::task::spawn_blocking(move || displays.refresh()).await {
                        Ok(Ok(_info)) => {}
                        Ok(Err(err)) => warn!(error = %err, "虚拟显示后台健康检查失败"),
                        Err(err) => warn!(error = %err, "虚拟显示后台健康检查任务失败"),
                    }
                }
            })
        });

        #[cfg(target_os = "linux")]
        let monitors = if let Some(display) = &capture_endpoint {
            PlatformCapturer::monitors_on_display(display).context("枚举虚拟显示失败")?
        } else {
            PlatformCapturer::monitors().context("枚举显示器失败")?
        };
        #[cfg(windows)]
        let monitors = {
            if virtual_displays.is_some() {
                anyhow::bail!("Windows 虚拟显示尚未接入 DXGI 采集端点；请先安装并验收 IddCx 驱动");
            }
            PlatformCapturer::monitors().context("枚举显示器失败")?
        };
        let monitor_index = if capture_endpoint.is_some() {
            0
        } else {
            opts.monitor
        };
        let monitor = monitors
            .iter()
            .find(|m| m.id == opts.monitor)
            .or_else(|| monitors.first())
            .with_context(|| {
                format!(
                    "找不到显示器索引 {}（共 {} 个）",
                    opts.monitor,
                    monitors.len()
                )
            })?
            .clone();
        // H.264 4:2:0 要求偶数尺寸。
        let width = monitor.w & !1;
        let height = monitor.h & !1;
        anyhow::ensure!(
            width >= 2 && height >= 2,
            "显示器尺寸非法：{}x{}",
            monitor.w,
            monitor.h
        );

        if let Some(displays) = &virtual_displays {
            println!(
                "[agent] 虚拟显示 ids={:?}，采集 id={} endpoint={}（物理显示器不变）",
                displays.ids(),
                displays.selected_id(),
                capture_endpoint.as_deref().unwrap_or("OS enumeration")
            );
        }

        let enc_cfg = dfy_codec::EncoderConfig {
            codec: opts.codec,
            width,
            height,
            fps: opts.fps,
            bitrate_kbps: opts.bitrate_kbps,
            keyframe_interval: opts.fps.saturating_mul(2).max(1),
            prefer_hardware: true,
        };

        // 先创建再丢弃一次编码器：尽早暴露「没有可用编码器」这类环境问题。
        drop(
            MftEncoderAdapter::new(&enc_cfg)
                .with_context(|| format!("创建 {} 编码器失败", enc_cfg.codec))?,
        );

        // 中继模式下：先与中继完成 tag 配对，再把配对后的 socket 交给 quinn；
        // 端点看到的对端是中继地址，TLS/会话语义完全不变。keepalive 由 RelaySocket 自带任务维持，
        // 因此这里要把 socket 句柄持有到函数结束。
        let mut relay_socket = None;
        let mut relay_endpoint: Option<quinn::Endpoint> = None;
        let (endpoint, identity) = if opts.auto_path {
            let relay = opts
                .relay
                .as_ref()
                .context("--auto-path 需要 --relay 或 --relay-profile 作为备用路径")?;
            anyhow::ensure!(!opts.p2p.is_some(), "--auto-path 暂不支持同时指定 --p2p");
            let identity = TlsIdentity::generate("dfy-agent").context("生成 Agent TLS 身份失败")?;
            let (direct, _) = bind_agent_with_identity(opts.addr, identity.clone())
                .with_context(|| format!("在 {} 上监听直连端点失败", opts.addr))?;
            let socket = relay.bind(Role::Agent).await.context("连接中继失败")?;
            println!(
                "[agent] 已与中继 {} 配对（tag {}）",
                relay.addr,
                crate::relay_bridge::tag_hex(&relay.tag)
            );
            let copy = socket.try_clone_socket().context("复制中继 socket 失败")?;
            let relay_ep = agent_endpoint_on_socket(copy, identity.clone())
                .context("用中继 socket 创建 Agent QUIC 端点失败")?;
            relay_socket = Some(socket);
            relay_endpoint = Some(relay_ep);
            println!("[agent] 双路径已就绪：直连 + 中继，等待可恢复会话迁移");
            (direct, identity)
        } else if let Some(relay) = opts.relay.as_ref() {
            let socket = relay.bind(Role::Agent).await.context("连接中继失败")?;
            println!(
                "[agent] 已与中继 {} 配对（tag {}）",
                relay.addr,
                crate::relay_bridge::tag_hex(&relay.tag)
            );
            let identity = TlsIdentity::generate("dfy-agent").context("生成 Agent TLS 身份失败")?;
            let copy = socket.try_clone_socket().context("复制中继 socket 失败")?;
            let endpoint = agent_endpoint_on_socket(copy, identity.clone())
                .context("用中继 socket 创建 Agent QUIC 端点失败")?;
            relay_socket = Some(socket);
            (endpoint, identity)
        } else if let Some(p2p) = opts.p2p.as_ref() {
            // P2P 直连：先按 STUN 结果收集候选并双端打洞，再把已连通的 socket 交给 quinn。
            let handoff = crate::p2p_bridge::prepare("agent", p2p)
                .await
                .context("P2P 打洞失败")?;
            println!(
                "[agent] P2P 就绪：本地端口 {}，候选 {} 个，NAT {:?}",
                handoff.local_port,
                handoff.candidates.len(),
                handoff.nat_hint
            );
            let identity = TlsIdentity::generate("dfy-agent").context("生成 Agent TLS 身份失败")?;
            let endpoint = agent_endpoint_on_socket(handoff.socket, identity.clone())
                .context("用打洞 socket 创建 Agent QUIC 端点失败")?;
            (endpoint, identity)
        } else {
            bind_agent(opts.addr)
                .with_context(|| format!("在 {} 上监听失败（UDP 端口被占用？）", opts.addr))?
        };
        let local = endpoint.local_addr().context("读取监听地址失败")?;
        println!("[agent] 监听 {local}（UDP/QUIC，ALPN = dfyp/1）");
        println!("[agent] 证书 SPKI-SHA256 = {}", identity.fingerprint_hex());
        println!("[agent] 短指纹 = {}", identity.fingerprint_short());
        println!(
            "[agent] 显示器 {}（{} {}x{}，DPI {}）→ {} {}kbps {}fps",
            monitor.id,
            monitor.name,
            width,
            height,
            monitor.dpi,
            enc_cfg.codec,
            opts.bitrate_kbps,
            opts.fps
        );
        println!("[agent] 等待客户端连接，Ctrl+C 退出");

        // 采集/编码线程和恢复授权跨连接复用，迁移令牌不能随单次握手对象丢失。
        let mut capture: Option<CaptureLoop> = None;
        let mut capture_handle: Option<CaptureHandle> = None;
        let mut sessions = tokio::task::JoinSet::new();
        let handler_template = if capture_endpoint.is_some() {
            #[cfg(target_os = "linux")]
            {
                AgentHandler::new_on_display_shared(
                    opts.allow_input,
                    opts.allow_clipboard,
                    opts.recv_dir.clone(),
                    live_capture_endpoint.clone().context("虚拟显示端点缺失")?,
                )
            }
            #[cfg(not(target_os = "linux"))]
            {
                AgentHandler::new(
                    opts.allow_input,
                    opts.allow_clipboard,
                    opts.recv_dir.clone(),
                )
            }
        } else {
            AgentHandler::new(
                opts.allow_input,
                opts.allow_clipboard,
                opts.recv_dir.clone(),
            )
        };
        let mut auth = ServerAuth::new(opts.password.expose(), "dfy-agent");
        let mut stats_tick = tokio::time::interval(Duration::from_secs(5));
        stats_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let coordinator = SessionCoordinator::default();
        auth.permission_bitmap = handler_template.permission_bitmap();
        if opts.audio_out {
            auth.permission_bitmap |= PERM_AUDIO_OUT;
        }
        if !handler_template.is_enabled() {
            auth.capabilities
                .retain(|capability| capability != "input-move.datagram.v1");
        }

        loop {
            while let Some(result) = sessions.try_join_next() {
                if let Err(err) = result {
                    warn!(error = %err, "会话任务异常结束");
                }
            }
            if let Some(displays) = &virtual_displays {
                let displays = Arc::clone(displays);
                tokio::task::spawn_blocking(move || displays.refresh()).await??;
            }
            let incoming = tokio::select! {
                incoming = endpoint.accept() => match incoming {
                    Some(incoming) => incoming,
                    None => break,
                },
                incoming = async {
                    match relay_endpoint.as_ref() {
                        Some(endpoint) => endpoint.accept().await,
                        None => std::future::pending().await,
                    }
                } => match incoming {
                    Some(incoming) => incoming,
                    None => break,
                },
                _ = tokio::signal::ctrl_c() => {
                    println!("\n[agent] 收到 Ctrl+C，关闭");
                    break;
                },
                _ = stats_tick.tick() => {
                    if let Some(capture) = &capture {
                        println!("[agent] 采集实时统计：{:?}", capture.stats());
                    }
                    continue;
                }
            };

            let conn = match incoming.await {
                Ok(conn) => conn,
                Err(err) => {
                    warn!(error = %err, "接受连接失败");
                    continue;
                }
            };
            let peer = conn.remote_address();
            println!("[agent] 新连接：{peer}");

            let handler = handler_template.clone();
            let handshake = match dfy_transport::server_handshake_context(&conn, &auth).await {
                Ok(handshake) => handshake,
                Err(err) => {
                    warn!(error = %err, "握手失败，丢弃连接");
                    continue;
                }
            };
            let dfy_transport::ServerHandshake {
                welcome,
                chan,
                hello,
                exporter,
            } = handshake;

            let caps = CaptureLoopConfig {
                capture_timeout_ms: 5,
                max_fps: opts.fps.max(1),
                queue_capacity: 16,
                max_consecutive_errors: 10,
            };
            // 采集线程在整个进程生命周期内只创建一次：断线重连 / 路径迁移时不必
            // 重建采集器与编码器（重建会丢一帧关键帧并重新探测显示器）。
            if capture.is_none() {
                let enc_cfg_for_loop = enc_cfg.clone();
                #[cfg(target_os = "linux")]
                let live_endpoint_for_capture = live_capture_endpoint.clone();
                let displays_for_capture = virtual_displays.clone();
                let initial = RuntimeSettings {
                    fps: opts.fps,
                    bitrate_kbps: opts.bitrate_kbps,
                    resolution: Some(dfy_session::EncodeResolution {
                        width: enc_cfg.width,
                        height: enc_cfg.height,
                    }),
                };
                let enc_cfg_for_rebuild = enc_cfg.clone();
                match start_capture_loop_with_encoder_factory(
                    caps,
                    initial,
                    move || {
                        #[cfg(target_os = "linux")]
                        let capturer: CliCapturer = if let (Some(displays), Some(live_endpoint)) =
                            (&displays_for_capture, &live_endpoint_for_capture)
                        {
                            let endpoint = live_endpoint
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .clone();
                            CliCapturer {
                                inner: Box::new(RecoveringVirtualCapturer {
                                    endpoint: endpoint.clone(),
                                    live_endpoint: Arc::clone(live_endpoint),
                                    displays: Arc::clone(displays),
                                    capturer: PlatformCapturer::on_display(
                                        &endpoint,
                                        monitor_index,
                                    )?,
                                    recovery_requested: false,
                                }),
                            }
                        } else {
                            CliCapturer {
                                inner: Box::new(PlatformCapturer::new(monitor_index)?),
                            }
                        };
                        #[cfg(not(target_os = "linux"))]
                        let capturer = {
                            let _ = &displays_for_capture;
                            let capturer = PlatformCapturer::new(monitor_index)?;
                            #[cfg(windows)]
                            println!(
                                "[agent] Windows 采集后端：{}，显示器 {} {}x{}",
                                capturer.backend_name(),
                                capturer.monitor().name,
                                capturer.monitor().w,
                                capturer.monitor().h
                            );
                            capturer
                        };
                        let encoder = MftEncoderAdapter::new(&enc_cfg_for_loop)?;
                        Ok((capturer, encoder))
                    },
                    move |settings| {
                        let mut encoder_cfg = enc_cfg_for_rebuild.clone();
                        encoder_cfg.width = settings.resolution.map_or(width, |value| value.width);
                        encoder_cfg.height =
                            settings.resolution.map_or(height, |value| value.height);
                        encoder_cfg.fps = settings.fps;
                        encoder_cfg.bitrate_kbps = settings.bitrate_kbps;
                        encoder_cfg.keyframe_interval = settings.fps.saturating_mul(2).max(1);
                        MftEncoderAdapter::new(&encoder_cfg)
                    },
                ) {
                    Ok(mut loop_handle) => {
                        capture_handle = Some(loop_handle.shared_handle());
                        capture = Some(loop_handle);
                    }
                    Err(err) => {
                        auth.resume_registry.revoke(&welcome.session_id);
                        warn!(error = %err, "启动采集线程失败");
                        continue;
                    }
                }
            }
            let capture = capture_handle.clone().expect("采集句柄刚刚已创建");
            let agent_cfg = AgentConfig {
                max_frames: opts.max_frames,
                idle_timeout: Duration::from_secs(3600),
                drain_delay: Duration::from_millis(100),
                stay_alive: true,
                migration: Some(MigrationContext {
                    auth: auth.clone(),
                    hello,
                    exporter,
                    coordinator: coordinator.clone(),
                }),
                coordinator: coordinator.clone(),
                adaptation: Some(dfy_session::StreamAdaptationConfig::default()),
            };

            println!(
                "[agent] 输入注入{}",
                if handler.is_enabled() {
                    "已启用"
                } else {
                    "未启用（只串流；未提供 --allow-input 或后端不可用）"
                }
            );
            println!(
                "[agent] 剪贴板同步{}",
                if handler.clipboard_enabled() {
                    "已启用（拉模式，500ms 轮询）"
                } else {
                    "未启用（未提供 --allow-clipboard 或后端不可用）"
                }
            );
            println!(
                "[agent] 文件接收{}",
                if handler.files_enabled() {
                    "已启用"
                } else {
                    "未启用（--recv-dir 未给出或不可用）"
                }
            );

            // 音频采集与视频会话独立：每连接一个任务，迁移后新连接会启动新的采集任务。
            let audio_stop = Arc::new(AtomicBool::new(false));
            if opts.audio_out {
                let conn_audio = conn.clone();
                let stop = Arc::clone(&audio_stop);
                let test_tone = opts.audio_test_tone;
                sessions.spawn(async move {
                    let config = dfy_audio::AudioConfig::default();
                    let mut source: Option<Box<dyn dfy_audio::AudioSource>> = if test_tone {
                        None
                    } else {
                        match dfy_audio::create_source(config) {
                            Ok(source) => Some(source),
                            Err(err) => {
                                warn!(error = %err, "音频采集后端不可用，本次会话不发送音频");
                                return;
                            }
                        }
                    };
                    println!(
                        "[agent] 音频已启用（{}，16kHz/单声道/20ms DATAGRAM）",
                        if test_tone {
                            "诊断正弦波"
                        } else {
                            "设备采集"
                        }
                    );
                    let mut seq = 0u32;
                    let mut phase = 0u32;
                    let per_frame = config.samples_per_frame();
                    while !stop.load(Ordering::Relaxed) {
                        let frame = if let Some(source) = source.as_mut() {
                            match source.next_frame() {
                                Ok(Some(frame)) => frame,
                                Ok(None) => {
                                    tokio::time::sleep(Duration::from_millis(1)).await;
                                    continue;
                                }
                                Err(err) => {
                                    warn!(error = %err, "音频采集失败");
                                    break;
                                }
                            }
                        } else {
                            let samples: Vec<i16> = (0..per_frame)
                                .map(|i| {
                                    let angle = f64::from(phase + i as u32) * 0.03;
                                    (angle.sin() * 8000.0) as i16
                                })
                                .collect();
                            phase = phase.wrapping_add(per_frame as u32);
                            dfy_audio::PcmFrame {
                                ts_us: dfy_transport::now_us(),
                                samples,
                            }
                        };
                        match frame.to_datagram(seq) {
                            Ok(bytes) => {
                                if conn_audio.send_datagram(bytes::Bytes::from(bytes)).is_err() {
                                    break;
                                }
                                seq = seq.wrapping_add(1);
                            }
                            Err(err) => warn!(error = %err, "音频帧编码失败"),
                        }
                        tokio::time::sleep(if test_tone {
                            Duration::from_millis(20)
                        } else {
                            Duration::from_millis(1)
                        })
                        .await;
                    }
                });
            }

            sessions.spawn(async move {
                let result = serve_agent_shared(conn, welcome, chan, &capture, handler.clone(), agent_cfg).await;
                audio_stop.store(true, Ordering::Relaxed);
                // 防卡键：会话结束（正常/异常）立刻释放本机可能仍按下的键。
                handler.release();
                let injected = handler.stats();
                let clipboard = handler.clipboard_stats();
                match result {
                    Ok(stats) => println!(
                        "[agent] 会话结束：帧 {}，{}，RTT {:?} ms，原因 {}；输入注入 {} 条（忽略 {}，失败 {}）",
                        stats.frames_sent,
                        dfy_transport::human_bytes(stats.bytes_sent),
                        stats.rtt_ms,
                        stats.end_reason,
                        injected.injected,
                        injected.ignored,
                        injected.failed
                    ),
                    Err(err) => warn!(error = %err, "会话异常结束"),
                }
                println!(
                    "[agent] 剪贴板：清单 {}↑/{}↓，拉取 {}↑/{}↓，分块 {}↑/{}↓，确认 {}↑/{}↓",
                    clipboard.lists_sent,
                    clipboard.lists_received,
                    clipboard.pulls_sent,
                    clipboard.pulls_received,
                    clipboard.chunks_sent,
                    clipboard.chunks_received,
                    clipboard.applied_sent,
                    clipboard.applied_received
                );
                let files = handler.file_stats();
                if files.offers_received > 0 || files.chunks_received > 0 {
                    println!(
                        "[agent] 文件：offer {}，块 {}（{}），确认 {}，结果 {}，完成 {}，失败 {}",
                        files.offers_received,
                        files.chunks_received,
                        dfy_transport::human_bytes(files.bytes),
                        files.acks_sent,
                        files.results_sent,
                        files.files_completed,
                        files.files_failed
                    );
                }
            });
        }

        #[cfg(target_os = "linux")]
        if let Some(monitor) = vdisplay_monitor {
            vdisplay_monitor_stop.store(true, Ordering::SeqCst);
            monitor.abort();
            let _ = monitor.await;
        }

        endpoint.close(0u32.into(), b"agent shutdown");
        drop(relay_socket);
        if let Some(capture) = capture.as_mut() {
            capture.stop();
        }
        while sessions.join_next().await.is_some() {}
        if let Some(mut capture) = capture {
            if let Some(result) = capture.join() {
                match result {
                    Ok(stats) => println!(
                        "[agent] 采集线程收尾：采集 {}，编码 {}，产出 {} 份",
                        stats.captured, stats.encoded, stats.frames_yielded
                    ),
                    Err(err) => warn!(error = %err, "采集线程异常结束"),
                }
            }
        }
        println!("[agent] 已退出");
        Ok(())
    }
}

/// 主控端统计。
#[derive(Debug, Default, Clone)]
pub struct ViewStats {
    /// 收到的视频 DATAGRAM 数。
    pub datagrams: u64,
    /// 收到的音频 DATAGRAM 数。
    pub audio_packets: u64,
    /// 成功解码并送出的音频帧数。
    pub audio_frames: u64,
    /// 最近一次音频序号（诊断丢包/乱序）。
    pub audio_last_seq: u32,
    /// 抖动缓冲确认的丢失帧数。
    pub audio_lost: u64,
    /// 乱序到达帧数。
    pub audio_reordered: u64,
    /// 重复帧数。
    pub audio_duplicates: u64,
    /// 抖动缓冲历史最大深度。
    pub audio_depth: usize,
    /// 因容量上限被淘汰的音频帧数。
    pub audio_evicted: u64,
    /// 收到的字节数。
    pub bytes: u64,
    /// 重组成完整帧的数目。
    pub frames: u64,
    /// 成功解码的帧数。
    pub decoded: u64,
    /// 会话结束时从解码器内部缓冲区排出的帧数。
    pub drained_frames: u64,
    /// 关键帧数。
    pub keyframes: u64,
    /// 解析失败数。
    pub malformed: u64,
    /// 已发送的接收质量反馈窗口数。
    pub feedback_windows_sent: u64,
    /// 心跳 RTT（毫秒）。
    pub rtt_ms: Option<f64>,
    /// 最后一次完整帧 ID（迁移进度凭据）。
    pub last_frame_id: u64,
    /// 最后一次完整帧序号。
    pub last_frame_seq: u64,
    /// 成功可恢复迁移次数。
    pub path_switches: u64,
    /// 是否收到过 SessionResumeOk。
    pub resume_ok: bool,
    /// 发出的鼠标移动 DATAGRAM 数（仅 --control）。
    pub input_moves: u64,
    /// 发出的其它输入事件数（键/按键/滚轮，仅 --control）。
    pub input_events: u64,
    /// 结束原因。
    pub end_reason: &'static str,
}

/// 连接 Agent，收流并解码 N 秒，打印统计。
/// 为 View 打开一条 QUIC 路径。中继 socket 必须随连接一起持有，否则保活任务被 Drop。
async fn open_view_path(
    opts: &ViewOptions,
    tls: dfy_transport::ClientTls,
    path: PathKind,
) -> Result<(
    quinn::Endpoint,
    quinn::Connection,
    Option<dfy_relay::RelaySocket>,
)> {
    match path {
        PathKind::Direct => {
            let (endpoint, conn) = connect(opts.addr, tls)
                .await
                .with_context(|| format!("连接 {} 失败", opts.addr))?;
            Ok((endpoint, conn, None))
        }
        PathKind::Relay => {
            let relay = opts
                .relay
                .as_ref()
                .context("自动路径切换需要 --relay 或 --relay-profile")?;
            let socket = relay
                .bind(Role::Client)
                .await
                .context("切换到中继时配对失败")?;
            let copy = socket.try_clone_socket().context("复制中继 socket 失败")?;
            let (endpoint, conn) = connect_on_socket(copy, tls, relay.addr)
                .await
                .with_context(|| format!("经中继 {} 连接失败", relay.addr))?;
            Ok((endpoint, conn, Some(socket)))
        }
    }
}

/// 由 QUIC 的 `lost_packets`/`sent_packets` 增量算丢包率（ppm）。
/// 分母为 0 时返回 0；结果夹在 `u32` 范围内供 `PathSample` 使用。
fn loss_ppm_from_deltas(lost_delta: u64, sent_delta: u64) -> u32 {
    lost_delta
        .saturating_mul(1_000_000)
        .checked_div(lost_delta.saturating_add(sent_delta))
        .unwrap_or(0)
        .min(u64::from(u32::MAX)) as u32
}

/// 在候选路径上做一次轻量 QUIC 探测：只发少量 DATAGRAM，不建立应用会话，
/// 因此不会抢占当前会话的代际、视频发送或剪贴板/文件状态。
/// RTT/丢包来自 QUIC 自身路径统计；探测不承载视频，`recv_bps` 用哨兵值
/// `u64::MAX` 表示“没有吞吐量数据”，避免被误判为带宽降档。
async fn probe_path(
    opts: &ViewOptions,
    tls: dfy_transport::ClientTls,
    target: PathKind,
    probe_bytes: usize,
) -> Result<PathSample> {
    let (_endpoint, conn, _relay_socket) = open_view_path(opts, tls, target).await?;
    let before = conn.stats().path;
    // 用接近 MTU 的探测包：只对小包敏感的瓶颈（只延迟大 QUIC 包的代理/队列）
    // 否则会把拥塞中的直连误判为健康。
    let mut payload = vec![0u8; probe_bytes.clamp(64, 1200)];
    for (i, byte) in payload.iter_mut().enumerate() {
        *byte = i as u8;
    }
    let payload = bytes::Bytes::from(payload);
    for _ in 0..8 {
        // 探测包不承载产品数据；发送失败只减少样本量，不使探测整体失败。
        if conn.send_datagram(payload.clone()).is_err() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(160)).await;
    let after = conn.stats().path;
    let lost_delta = after.lost_packets.saturating_sub(before.lost_packets);
    let sent_delta = after.sent_packets.saturating_sub(before.sent_packets);
    let rtt_ms = (after.rtt.as_secs_f64() * 1000.0) as f32;
    conn.close(0u32.into(), b"path probe done");
    Ok(PathSample {
        at_ms: dfy_transport::now_us() / 1000,
        rtt_ms,
        loss_ppm: loss_ppm_from_deltas(lost_delta, sent_delta),
        recv_bps: u64::MAX,
        stall_ms: 0,
    })
}

type ResumedPath = (
    quinn::Endpoint,
    quinn::Connection,
    Option<dfy_relay::RelaySocket>,
    dfy_proto::msg::Welcome,
    dfy_transport::CtrlChannel,
);

/// 建立一条新路径后，用已有会话令牌做可恢复握手，而不是新建会话。
async fn resume_on_path(
    opts: &ViewOptions,
    tls: dfy_transport::ClientTls,
    target: PathKind,
    signing_key: &SigningKey,
    previous_welcome: &dfy_proto::msg::Welcome,
    last_frame_id: u64,
    last_frame_seq: u64,
) -> Result<ResumedPath> {
    let (endpoint, conn, socket) = open_view_path(opts, tls, target).await?;
    // 弱网/ virtualization（QEMU TCG）下新路径的握手往返可能明显超过 5s；
    // 过短的超时会把本来可用的迁移误判为失败并放弃已建立的 QUIC 连接。
    let (welcome, chan) = tokio::time::timeout(
        Duration::from_secs(15),
        client_resume_handshake(
            &conn,
            opts.password.expose(),
            "dfy-view",
            signing_key,
            previous_welcome,
            last_frame_id,
            last_frame_seq,
        ),
    )
    .await
    .context("会话迁移握手超时")?
    .context("会话迁移握手失败")?;
    Ok((endpoint, conn, socket, welcome, chan))
}

pub async fn run_view(opts: ViewOptions) -> Result<ViewStats> {
    let tls = client_tls(opts.expect_fingerprint).context("构造客户端 TLS 失败")?;
    let initial_path = if opts.auto_path {
        opts.auto_initial_path.unwrap_or(PathKind::Direct)
    } else if opts.relay.is_some() {
        PathKind::Relay
    } else {
        PathKind::Direct
    };
    if opts.auto_path {
        anyhow::ensure!(
            opts.relay.is_some(),
            "--auto-path 需要 --relay 或 --relay-profile 作为备用路径"
        );
        anyhow::ensure!(!opts.p2p.is_some(), "--auto-path 暂不支持同时指定 --p2p");
    }
    let (mut endpoint, mut conn, mut relay_socket) = if opts.auto_path {
        let pair = open_view_path(&opts, tls.clone(), initial_path).await?;
        if initial_path == PathKind::Relay {
            if let Some(relay) = opts.relay.as_ref() {
                println!(
                    "[view] 已与中继 {} 配对（tag {}）",
                    relay.addr,
                    crate::relay_bridge::tag_hex(&relay.tag)
                );
            }
        }
        pair
    } else if let Some(relay) = opts.relay.as_ref() {
        let pair = open_view_path(&opts, tls.clone(), PathKind::Relay).await?;
        println!(
            "[view] 已与中继 {} 配对（tag {}）",
            relay.addr,
            crate::relay_bridge::tag_hex(&relay.tag)
        );
        pair
    } else if let Some(p2p) = opts.p2p.as_ref() {
        let handoff = crate::p2p_bridge::prepare("view", p2p)
            .await
            .context("P2P 打洞失败")?;
        println!(
            "[view] P2P 就绪：本地端口 {}，候选 {} 个，NAT {:?}",
            handoff.local_port,
            handoff.candidates.len(),
            handoff.nat_hint
        );
        let pair = connect_on_socket(handoff.socket, tls.clone(), handoff.peer)
            .await
            .with_context(|| format!("经 P2P 对端 {} 连接失败", handoff.peer))?;
        (pair.0, pair.1, None)
    } else {
        open_view_path(&opts, tls.clone(), PathKind::Direct).await?
    };
    println!(
        "[view] 已连接 {}",
        if initial_path == PathKind::Relay {
            "（经中继）"
        } else {
            "（直连）"
        }
    );
    if let Some(observed) = tls.observed_hex() {
        println!("[view] Agent 证书 SPKI-SHA256 = {observed}");
    }

    let signing_key = SigningKey::generate(&mut rand::rngs::OsRng);
    let device_public_key = signing_key.verifying_key().to_bytes();
    let (welcome, chan) = client_handshake(
        &conn,
        opts.password.expose(),
        "dfy-view",
        device_public_key.to_vec(),
    )
    .await
    .context("握手失败")?;
    let mut session_welcome = welcome.clone();
    let permissions = welcome.permission_bitmap;
    anyhow::ensure!(
        permissions & PERM_VIEW != 0,
        "Agent 未授予查看权限，拒绝开始会话"
    );
    let control_allowed = permissions & PERM_CONTROL != 0;
    let clipboard_allowed = permissions & PERM_CLIPBOARD != 0;
    let files_allowed = permissions & PERM_FILE != 0;
    if opts.control && !control_allowed {
        println!("[view] Agent 未授予控制权限，本次不发送输入");
    }
    if opts.clipboard && !clipboard_allowed {
        println!("[view] Agent 未授予剪贴板权限，本次不同步剪贴板");
    }
    if !opts.send_files.is_empty() && !files_allowed {
        println!("[view] Agent 未授予文件权限，本次不发送文件");
    }
    println!(
        "[view] 握手成功：agent = {}，能力 {} 项，权限位图 0x{:08x}",
        welcome.agent_name,
        welcome.capability.len(),
        welcome.permission_bitmap
    );

    let ClientSession {
        conn: session_conn,
        welcome: _session_welcome,
        tx,
        rx,
        heartbeat,
    } = ClientSession::new(conn.clone(), welcome, chan);
    conn = session_conn;
    let mut tx = tx;
    let quality_control = opts.quality_control()?;
    if let Some(request) = quality_control {
        anyhow::ensure!(
            control_allowed,
            "Agent 未授予控制权限，拒绝发送画质控制请求"
        );
        anyhow::ensure!(
            session_welcome
                .capability
                .iter()
                .any(|capability| capability == CAPABILITY_QUALITY_CONTROL),
            "Agent 未声明 {} 能力，拒绝发送画质控制请求",
            CAPABILITY_QUALITY_CONTROL
        );
        tx.send_ctrl(&request)
            .await
            .context("发送画质控制请求失败")?;
        println!("[view] 已发送画质控制请求");
    }
    let mut hb = heartbeat;
    let mut rx = rx;
    let mut decoder = MftDecoderAdapter::with_codec(opts.codec, true)
        .with_context(|| format!("创建 {} 解码器失败", opts.codec))?;
    let mut assembler = FrameAssembler::new();
    let mut feedback = StreamFeedbackWindow::new(std::time::Instant::now());
    let feedback_enabled = session_welcome
        .capability
        .iter()
        .any(|name| name == "stream-feedback.v1");
    let mut next_feedback = Instant::now() + FEEDBACK_INTERVAL;
    let mut feedback_dropped_reported = 0u64;
    let mut stats = ViewStats::default();
    let mut path_monitor = if opts.auto_path {
        Some(PathMonitor::with_initial_path(
            PathConfig::default(),
            initial_path,
        ))
    } else {
        None
    };
    let mut active_path = initial_path;
    let mut next_path_sample = Instant::now() + Duration::from_millis(250);
    let mut last_sample_at = Instant::now();
    let mut last_sample_bytes = 0u64;
    let mut last_frame_at = Instant::now();
    let mut forced_switch_done = false;
    let mut last_lost_packets = conn.stats().path.lost_packets;
    let mut last_sent_packets = conn.stats().path.sent_packets;
    // 候选路径探测可能返回 SwitchTo；下一个采样 tick 用同一套切换逻辑执行它。
    let mut deferred_action: Option<Action> = None;
    let mut pending_probe: Option<(PathKind, tokio::task::JoinHandle<Result<PathSample>>)> = None;
    let mut pending_migration: Option<(
        PathKind,
        Instant,
        tokio::task::JoinHandle<Result<ResumedPath>>,
    )> = None;
    let mut last_decode_ms: Option<f64> = None;
    let mut last_ui_stats_at = Instant::now();
    let mut last_ui_bytes = 0u64;
    let mut last_ui_lost = conn.stats().path.lost_packets;
    let mut last_ui_sent = conn.stats().path.sent_packets;
    let mut next_ui_stats = Instant::now() + Duration::from_millis(500);
    let deadline = Instant::now() + Duration::from_secs(opts.seconds.max(1));
    let mut saved = false;
    let mut next_ping = Instant::now();

    // 剪贴板同步（默认关闭；--clipboard 开启。平台不支持时自动降级）。
    let mut clipboard = if opts.clipboard && clipboard_allowed {
        ViewClipboard::new()
    } else {
        None
    };
    if clipboard.is_some() {
        println!("[view] 剪贴板同步已启用（拉模式，500ms 轮询本地剪贴板）");
    }
    let mut next_clipboard_poll = Instant::now() + crate::clipboard_bridge::CLIPBOARD_POLL;
    // 音频抖动缓冲：按序号重排并吸收 20ms 抖动；播放由独立 tick 驱动。
    let mut audio_buffer = dfy_audio::AudioJitterBuffer::new(100);
    let mut next_audio_tick = Instant::now() + Duration::from_millis(20);

    // 音频播放（--audio-in）：设备不可用时降级为 Null，不中断视频会话。
    let mut audio: Option<Box<dyn dfy_audio::AudioSink>> = if opts.audio_in {
        match dfy_audio::create_sink(dfy_audio::AudioConfig::default()) {
            Ok(sink) => {
                println!(
                    "[view] 音频播放已启用（音量 {}%{}）",
                    opts.audio_volume.min(100),
                    if opts.audio_mute { "，静音" } else { "" }
                );
                Some(Box::new(dfy_audio::GainSink::new(
                    sink,
                    opts.audio_volume,
                    opts.audio_mute,
                )) as Box<dyn dfy_audio::AudioSink>)
            }
            Err(err) => {
                warn!(error = %err, "音频输出设备不可用，继续只收视频");
                None
            }
        }
    } else {
        None
    };

    // 文件发送（--send-file 开启）：进入会话后逐个 offer。
    let mut files = if opts.send_files.is_empty() || !files_allowed {
        None
    } else {
        Some(ViewFiles::new(opts.send_files.clone()))
    };
    if files.is_some() {
        println!(
            "[view] 文件发送已启用（{} 个路径；复用 control 流，256KiB/块）",
            opts.send_files.len()
        );
    }
    if let Some(view_files) = files.as_mut() {
        for env in view_files.start_next() {
            let _ = tx.send_ctrl(&env).await;
        }
    }
    let mut next_file_tick = Instant::now() + Duration::from_millis(100);

    // 图形窗口上屏（--ui）：解码线程把 BGRA 帧推给 UI 线程；窗口键盘/鼠标事件转成输入事件。
    let mut ui = if opts.ui {
        let config = ViewerConfig::new("大肥鱼远程")
            .with_stats(true)
            .with_fullscreen(opts.fullscreen);
        match spawn_viewer(config) {
            Ok(handle) => {
                println!(
                    "[view] UI 窗口已启动（F11 全屏，Esc 关闭，Ctrl+Alt+Shift+Q 切换远控输入）"
                );
                Some(handle)
            }
            Err(err) => {
                println!("[view] UI 窗口不可用：{err:#}；继续只打印统计");
                None
            }
        }
    } else {
        None
    };
    // UI 窗口自己产生键盘/鼠标事件，因此 --ui 时不再装本地低级钩子。
    let mut ui_send_input = control_allowed;

    // 控制输入采集（Windows 低级钩子 / Linux X11 XInput2）；不可用时降级为「只收视频」。
    let mut capture = if opts.control && control_allowed && ui.is_none() {
        match InputCapture::start() {
            Ok(capture) => {
                println!("[view] 控制输入已启用（鼠标移动 10ms 合并；Ctrl+Alt+Shift+Q 停止）");
                capture
            }
            Err(err) => {
                println!("[view] 控制输入不可用：{err:#}；继续只收视频");
                InputCapture::disabled()
            }
        }
    } else {
        InputCapture::disabled()
    };
    if ui.is_some() && opts.control && control_allowed {
        println!(
            "[view] --ui 与 --control 同时给出：键盘/鼠标由 UI 窗口转发，本地全局输入采集不启用"
        );
    }

    loop {
        if Instant::now() >= deadline {
            stats.end_reason = "duration";
            break;
        }
        tokio::select! {
                frame = rx.recv_ctrl() => {
                    match frame {
                        Ok(Some(env)) => match &env.body {
                            Some(envelope::Body::Ping(_)) => {
                                if let Some(pong) = hb.on_frame(&env) {
                                    let _ = tx.send_ctrl(&pong).await;
                                }
                            }
                            Some(envelope::Body::Pong(_)) => {
                                hb.on_frame(&env);
                            }
                            Some(envelope::Body::VideoConfig(config)) => {
                                println!(
                                    "[view] Agent 应用视频质量：{}x{} {}fps {}kbps",
                                    config.width, config.height, config.fps, config.bitrate_kbps
                                );
                            }
                            Some(envelope::Body::Bye(_)) => {
                                stats.end_reason = "agent_bye";
                                break;
                            }
                            _ => {
                                if is_file_envelope(&env) {
                                    if let Some(view_files) = files.as_mut() {
                                        for reply in view_files.on_envelope(&env) {
                                            let _ = tx.send_ctrl(&reply).await;
                                        }
                                    }
                                } else if let Some(sync) = clipboard.as_mut() {
                                    let replies = sync.on_ctrl(&env);
                                    for reply in replies {
                                        let _ = tx.send_ctrl(&reply).await;
                                    }
                                }
                            }
                        },
                        Ok(None) => { stats.end_reason = "ctrl_closed"; break; }
                        Err(_) => { stats.end_reason = "ctrl_error"; break; }
                    }
                }

                datagram = conn.read_datagram() => {
                    match datagram {
                        Ok(bytes) => {
                            stats.datagrams += 1;
                            stats.bytes += bytes.len() as u64;
                            if bytes.first() == Some(&dfy_proto::CHAN_AUDIO) {
                                stats.audio_packets += 1;
                                if audio.is_some() {
                                    match dfy_audio::PcmFrame::from_datagram(&bytes) {
                                        Ok(Some((seq, frame))) => {
                                            stats.audio_last_seq = seq;
                                            if let Err(err) = audio_buffer.push(
                                                seq,
                                                frame,
                                                dfy_transport::now_us(),
                                            ) {
                                                debug!(error = %err, "音频入抖动缓冲失败");
                                            }
                                        }
                                        Ok(None) => {}
                                        Err(err) => debug!(error = %err, "音频帧解码失败"),
                                    }
                                }
                            } else {
                            match dfy_proto::decode_video(&bytes) {
                                Ok((header, payload)) => {
                                    feedback.note_packet(std::time::Instant::now(), payload.len());
                                    if header.flags & dfy_proto::consts::VIDEO_FLAG_KEY != 0 {
                                        stats.keyframes += 1;
                                    }
                                    match assembler.push(&header, payload) {
                                        Ok(Some(frame)) => {
                                            stats.frames += 1;
                                            stats.last_frame_id = u64::from(frame.frame_id);
                                            stats.last_frame_seq = u64::from(frame.seq);
                                            last_frame_at = Instant::now();
                                            feedback.note_assembled();
                                            let decode_started = std::time::Instant::now();
                                            match decoder.decode(&frame.data, frame.ts_us as u64) {
                                                Ok(Some(decoded)) => {
                                                    last_decode_ms = Some(
                                                        decode_started.elapsed().as_secs_f64() * 1000.0,
                                                    );
                                                    stats.decoded += 1;
                                                    feedback.note_delivered();
                                                    if let Some(handle) = ui.as_ref() {
                                                        handle.present.present(
                                                            decoded.width,
                                                            decoded.height,
                                                            decoded.data.clone(),
                                                        );
                                                    }
                                                    if !saved {
                                                        if let Some(path) = &opts.save_first_frame {
                                                            let mut buf = Vec::with_capacity(decoded.data.len());
                                                            buf.extend_from_slice(&decoded.data);
                                                            std::fs::write(path, &buf)
                                                                .with_context(|| format!("写 {} 失败", path.display()))?;
                                                            println!("[view] 首帧已保存：{}（{}x{}，{} 字节）",
                                                                path.display(), decoded.width, decoded.height, buf.len());
                                                            saved = true;
                                                        }
                                                    }
                                                }
                                                Ok(None) => {}
                                                Err(err) => debug!(error = %err, "解码失败"),
                                            }
                                        }
                                        Ok(None) => {}
                                        Err(err) => {
                                            stats.malformed += 1;
                                            debug!(error = %err, "分片重组失败");
                                        }
                                    }
                                }
                                Err(err) => {
                                    stats.malformed += 1;
                                    debug!(error = %err, "解析视频头失败");
                                }
                            }
                            }
                        }
                        Err(_) => { stats.end_reason = "conn_closed"; break; }
                    }
                }

                msg = capture.recv() => {
                    match msg {
                        Some(CtrlMsg::Move(ev)) => match send_input_move(&conn, &ev) {
                            Ok(()) => stats.input_moves += 1,
                            Err(err) => debug!(error = %err, "发送鼠标移动 DATAGRAM 失败"),
                        },
                        Some(CtrlMsg::Event(env)) => {
                            if tx.send_ctrl(&env).await.is_ok() {
                                stats.input_events += 1;
                            }
                        }
                        Some(CtrlMsg::Stopped) => {
                            println!("[view] 收到 Ctrl+Alt+Shift+Q：停止采集控制输入（继续收视频）");
                            if let Some((_, handle)) = pending_probe.take() {
            handle.abort();
        }
        if let Some((_, _, handle)) = pending_migration.take() {
            handle.abort();
        }
        capture.stop();
                        }
                        None => {
                            debug!("控制输入采集已结束");
                            if let Some((_, handle)) = pending_probe.take() {
            handle.abort();
        }
        if let Some((_, _, handle)) = pending_migration.take() {
            handle.abort();
        }
        capture.stop();
                        }
                    }
                }

                ui_event = async {
                    match ui.as_mut() {
                        Some(handle) => handle.event_rx.recv().await,
                        None => std::future::pending::<Option<ViewerEvent>>().await,
                    }
                } => {
                    match ui_event {
                        Some(ViewerEvent::Key {
                            usage,
                            down,
                            modifiers,
                        }) => {
                            if ui_send_input {
                                let env = input_key_envelope(usage, down, modifiers);
                                if tx.send_ctrl(&env).await.is_ok() {
                                    stats.input_events += 1;
                                }
                            }
                        }
                        Some(ViewerEvent::Mouse {
                            x,
                            y,
                            width,
                            height,
                            button,
                            down,
                        }) => {
                            if !ui_send_input {
                                continue;
                            }
                            if let Some(button) = button {
                                let env = input_button_envelope(button, down);
                                if tx.send_ctrl(&env).await.is_ok() {
                                    stats.input_events += 1;
                                }
                            } else if let Some(ev) = frame_point_to_move(x, y, width, height) {
                                match send_input_move(&conn, &ev) {
                                    Ok(()) => stats.input_moves += 1,
                                    Err(err) => debug!(error = %err, "发送鼠标移动 DATAGRAM 失败"),
                                }
                            }
                        }
                        Some(ViewerEvent::Wheel { dx, dy }) => {
                            if ui_send_input {
                                let env = input_wheel_envelope(dx, dy);
                                if tx.send_ctrl(&env).await.is_ok() {
                                    stats.input_events += 1;
                                }
                            }
                        }
                        Some(ViewerEvent::Closed) => {
                            stats.end_reason = "ui_closed";
                            break;
                        }
                        Some(ViewerEvent::ToggleControl) => {
                            if control_allowed {
                                ui_send_input = !ui_send_input;
                                println!(
                                    "[view] 远控输入：{}",
                                    if ui_send_input { "开启" } else { "暂停" }
                                );
                            }
                        }
                        Some(ViewerEvent::ToggleFullscreen) => {}
                        None => {
                            stats.end_reason = "ui_closed";
                            break;
                        }
                    }
                }

                _ = tokio::time::sleep_until(next_clipboard_poll) => {
                    next_clipboard_poll = Instant::now() + crate::clipboard_bridge::CLIPBOARD_POLL;
                    if let Some(sync) = clipboard.as_mut() {
                        for env in sync.poll() {
                            let _ = tx.send_ctrl(&env).await;
                        }
                    }
                }

                _ = tokio::time::sleep_until(next_audio_tick) => {
                    // 不追赶到未来：长时间停顿后追赶会把后续多个 tick 一次跳过，
                    // 反而让抖动缓冲在迁移前堆满。停顿后从当前时刻重新起拍。
                    next_audio_tick = Instant::now() + Duration::from_millis(20);
                    let queued = audio_buffer.len();
                    audio_buffer.adapt(queued);
                    let now_us = dfy_transport::now_us();
                    while let Some((_seq, frame)) = audio_buffer.pop_ready(now_us) {
                        if let Some(sink) = audio.as_mut() {
                            match sink.play(&frame) {
                                Ok(()) => stats.audio_frames += 1,
                                Err(err) => debug!(error = %err, "音频播放失败"),
                            }
                        }
                    }
                    let audio_stats = audio_buffer.stats();
                    if audio_buffer.len() > 8 {
                        debug!(
                            depth = audio_buffer.len(),
                            oldest_ts = audio_buffer.oldest_ts(),
                            now_us,
                            due = now_us.saturating_sub(100_000),
                            "音频抖动缓冲积压"
                        );
                    }
                    stats.audio_lost = audio_stats.lost;
                    stats.audio_reordered = audio_stats.reordered;
                    stats.audio_duplicates = audio_stats.duplicates;
                    stats.audio_depth = audio_stats.deepest;
                    stats.audio_evicted = audio_stats.evicted;
                }

                _ = tokio::time::sleep_until(next_file_tick) => {
                    next_file_tick = Instant::now() + Duration::from_millis(100);
                    if let Some(view_files) = files.as_mut() {
                        for env in view_files.start_next() {
                            let _ = tx.send_ctrl(&env).await;
                        }
                        for env in view_files.poll_chunks() {
                            let _ = tx.send_ctrl(&env).await;
                        }
                    }
                }

                _ = tokio::time::sleep_until(next_feedback) => {
                    if feedback_enabled {
                        feedback.set_dropped_frames(
                            assembler.dropped().saturating_sub(feedback_dropped_reported),
                        );
                        feedback_dropped_reported = assembler.dropped();
                        if let Some(window) = feedback.take(std::time::Instant::now()) {
                            if tx.send_ctrl(&dfy_proto::Envelope::stream_feedback(window)).await.is_err() {
                                stats.end_reason = "ctrl_closed";
                                break;
                            }
                            stats.feedback_windows_sent += 1;
                        }
                    }
                    next_feedback = Instant::now() + FEEDBACK_INTERVAL;
                }

                _ = tokio::time::sleep_until(next_path_sample) => {
                    if let Some(monitor) = path_monitor.as_mut() {
                        let now = Instant::now();
                        let elapsed = now.duration_since(last_sample_at).as_secs_f64().max(0.001);
                        let bytes_delta = stats.bytes.saturating_sub(last_sample_bytes);
                        let quic_stats = conn.stats();
                        let lost_delta = quic_stats.path.lost_packets.saturating_sub(last_lost_packets);
                        let sent_delta = quic_stats.path.sent_packets.saturating_sub(last_sent_packets);
                        let loss_ppm = loss_ppm_from_deltas(lost_delta, sent_delta);
                        last_lost_packets = quic_stats.path.lost_packets;
                        last_sent_packets = quic_stats.path.sent_packets;
                        let sample = PathSample {
                            rtt_ms: hb.rtt_ms().unwrap_or(f32::NAN as f64) as f32,
                            loss_ppm,
                            recv_bps: (bytes_delta as f64 * 8.0 / elapsed) as u64,
                            stall_ms: last_frame_at.elapsed().as_millis().min(u64::MAX as u128) as u64,
                            at_ms: dfy_transport::now_us() / 1000,
                        };
                        last_sample_at = now;
                        last_sample_bytes = stats.bytes;
                        next_path_sample = Instant::now() + Duration::from_millis(250);
                        let action = {
                            let action = if let Some(deferred) = deferred_action.take() {
                                deferred
                            } else if let Some(target) = opts.force_switch_once {
                                if !forced_switch_done {
                                    forced_switch_done = true;
                                    Action::SwitchTo(target)
                                } else {
                                    monitor.on_sample_for(active_path, sample)
                                }
                            } else {
                                monitor.on_sample_for(active_path, sample)
                            };
                            for event in monitor.drain_events() {
                                debug!("path event: {event:?}");
                            }
                            action
                        };
                        if !action.is_none() {
                            info!(?action, active = ?active_path, "路径监测输出决策");
                        }
                        match action {
                            Action::None => {}
                            Action::RequestProbe(target) => {
                                if pending_probe.is_none() && pending_migration.is_none() {
                                    info!(probe_path = ?target, "路径监测请求候选路径探测");
                                    let probe_opts = opts.clone();
                                    let probe_tls = tls.clone();
                                    let probe_bytes = opts.probe_bytes;
                                    let handle = tokio::spawn(async move {
                                        probe_path(&probe_opts, probe_tls, target, probe_bytes).await
                                    });
                                    pending_probe = Some((target, handle));
                                }
                            }
                            Action::SwitchTo(target) => {
                                if target != active_path && pending_migration.is_none() {
                                    let m_opts = opts.clone();
                                    let m_tls = tls.clone();
                                    let m_key = signing_key.clone();
                                    let m_welcome = session_welcome.clone();
                                    let frame_id = stats.last_frame_id;
                                    let frame_seq = stats.last_frame_seq;
                                    let started = Instant::now();
                                    let handle = tokio::spawn(async move {
                                        resume_on_path(
                                            &m_opts,
                                            m_tls,
                                            target,
                                            &m_key,
                                            &m_welcome,
                                            frame_id,
                                            frame_seq,
                                        )
                                        .await
                                    });
                                    pending_migration = Some((target, started, handle));
                                }
                            }
                        }
                    }
                }

                probe_result = async {
                    match pending_probe.as_mut() {
                        Some((_, handle)) => handle.await,
                        None => std::future::pending().await,
                    }
                } => {
                    let (target, _) = pending_probe.take().expect("pending_probe active");
                    match probe_result {
                        Ok(Ok(sample)) => {
                            info!(
                                probe_path = ?target,
                                rtt_ms = sample.rtt_ms,
                                loss_ppm = sample.loss_ppm,
                                "候选路径探测完成"
                            );
                            if let Some(monitor) = path_monitor.as_mut() {
                                let action = monitor.on_sample_for(target, sample);
                                for event in monitor.drain_events() {
                                    debug!("path event: {event:?}");
                                }
                                if !action.is_none() {
                                    debug!(?action, "探测采样触发路径决策，下一采样 tick 执行");
                                    deferred_action = Some(action);
                                }
                            }
                        }
                        Ok(Err(err)) => {
                            debug!(error = %err, probe_path = ?target, "候选路径探测失败");
                        }
                        Err(err) => {
                            debug!(error = %err, probe_path = ?target, "候选路径探测任务异常");
                        }
                    }
                }

                migration_result = async {
                    match pending_migration.as_mut() {
                        Some((_, _, handle)) => handle.await,
                        None => std::future::pending().await,
                    }
                } => {
                    let (target, started, _) = pending_migration.take().expect("pending_migration active");
                    match migration_result {
                        Ok(Ok((new_endpoint, new_conn, new_socket, new_welcome, new_chan))) => {
                            // 旧连接的音频属于旧 seq 纪元；握手期间积压的包无法与新
                            // 纪元拼接。Connection 被替换后由 QUIC 丢弃，不塞进缓冲。
                            info!(
                                queued = audio_buffer.len(),
                                "迁移时清空旧音频序号的缓冲"
                            );
                            // Agent 在新连接上重建音频任务，序号从 0 重新开始。
                            audio_buffer.reset_sequence();
                            let ClientSession {
                                conn: resumed_conn,
                                welcome: _,
                                tx: new_tx,
                                rx: new_rx,
                                heartbeat: new_hb,
                            } = ClientSession::new(new_conn.clone(), new_welcome.clone(), new_chan);
                            endpoint = new_endpoint;
                            conn = resumed_conn;
                            relay_socket = new_socket;
                            tx = new_tx;
                            rx = new_rx;
                            hb = new_hb;
                            // 迁移瞬间旧控制通道上可能刚好丢了 KeyUp；在新通道重放
                            // 当前仍按住的键/按钮。注入端对重复 Down 幂等。
                            let held = capture.held();
                            let mut replayed = 0usize;
                            for usage in held.keys {
                                if tx.send_ctrl(&input_key_envelope(usage, true, 0)).await.is_ok() {
                                    replayed += 1;
                                }
                            }
                            for button in held.buttons {
                                if tx.send_ctrl(&input_button_envelope(button, true)).await.is_ok() {
                                    replayed += 1;
                                }
                            }
                            if replayed > 0 {
                                info!(replayed, "迁移后已在新通道重放按住的输入状态");
                            }
                            session_welcome = new_welcome;
                            last_lost_packets = conn.stats().path.lost_packets;
                            last_sent_packets = conn.stats().path.sent_packets;
                            last_ui_lost = last_lost_packets;
                            last_ui_sent = last_sent_packets;
                            active_path = target;
                            assembler = FrameAssembler::new();
                            feedback_dropped_reported = assembler.dropped();
                            stats.path_switches += 1;
                            stats.resume_ok = true;
                            let took_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
                            let at_ms = dfy_transport::now_us() / 1000;
                            if let Some(monitor) = path_monitor.as_mut() {
                                monitor.on_switch_committed(at_ms, took_ms);
                            }
                            println!(
                                "[view] 会话已迁移到{}（epoch={}, 耗时 {}ms，帧=#{}）",
                                if target == PathKind::Relay { "中继" } else { "直连" },
                                session_welcome.session_epoch,
                                took_ms,
                                stats.last_frame_id
                            );
                        }
                        Ok(Err(err)) => {
                            tracing::warn!(error = %err, "路径迁移失败，保持当前路径");
                            let at_ms = dfy_transport::now_us() / 1000;
                            if let Some(monitor) = path_monitor.as_mut() {
                                monitor.on_switch_failed(at_ms);
                            }
                        }
                        Err(err) => {
                            tracing::warn!(error = %err, "路径迁移任务异常，保持当前路径");
                            let at_ms = dfy_transport::now_us() / 1000;
                            if let Some(monitor) = path_monitor.as_mut() {
                                monitor.on_switch_failed(at_ms);
                            }
                        }
                    }
                }

                _ = tokio::time::sleep_until(next_ui_stats) => {
                    next_ui_stats = Instant::now() + Duration::from_millis(500);
                    if let Some(handle) = ui.as_ref() {
                        let now = Instant::now();
                        let elapsed = now.duration_since(last_ui_stats_at).as_secs_f64().max(0.001);
                        let bytes_delta = stats.bytes.saturating_sub(last_ui_bytes);
                        let quic_stats = conn.stats();
                        let lost_delta = quic_stats.path.lost_packets.saturating_sub(last_ui_lost);
                        let sent_delta = quic_stats.path.sent_packets.saturating_sub(last_ui_sent);
                        last_ui_stats_at = now;
                        last_ui_bytes = stats.bytes;
                        last_ui_lost = quic_stats.path.lost_packets;
                        last_ui_sent = quic_stats.path.sent_packets;
                        handle.present.update_stats(dfy_ui::ConnectionStats {
                            rtt_ms: hb.rtt_ms(),
                            loss_ppm: Some(loss_ppm_from_deltas(lost_delta, sent_delta)),
                            bitrate_kbps: Some(((bytes_delta as f64 * 8.0 / elapsed) / 1000.0) as u32),
                            decode_ms: last_decode_ms,
                            path: if active_path == PathKind::Relay {
                                "Relay".to_string()
                            } else {
                                "Direct".to_string()
                            },
                            relay_name: if active_path == PathKind::Relay {
                                opts.relay.as_ref().map(|r| r.addr.to_string())
                            } else {
                                None
                            },
                        });
                    }
                }

                _ = tokio::time::sleep_until(next_ping) => {
                    let ts = dfy_transport::now_us();
                    if tx.send_ctrl(&dfy_proto::Envelope::ping(ts)).await.is_ok() {
                        hb.mark_ping_sent(ts);
                    }
                    next_ping = Instant::now() + hb.interval;
                    if hb.timed_out() {
                        stats.end_reason = "timeout";
                        break;
                    }
                }
            }
    }

    if let Some((_, handle)) = pending_probe.take() {
        handle.abort();
    }
    if let Some((_, _, handle)) = pending_migration.take() {
        handle.abort();
    }
    capture.stop();
    stats.rtt_ms = hb.rtt_ms();
    match decoder.flush() {
        Ok(frames) => {
            stats.drained_frames = frames.len() as u64;
            stats.decoded += stats.drained_frames;
            for decoded in frames {
                if let Some(handle) = ui.as_ref() {
                    handle
                        .present
                        .present(decoded.width, decoded.height, decoded.data);
                }
            }
        }
        Err(err) => debug!(error = %err, "解码器收尾排空失败"),
    }
    let _ = tx
        .send_ctrl(&dfy_proto::Envelope::bye(dfy_proto::BYE_NORMAL))
        .await;
    let _ = tx.finish();
    conn.close(0u32.into(), b"view done");
    if let Some(handle) = ui.take() {
        handle.close();
    }

    endpoint.close(0u32.into(), b"view done");
    drop(relay_socket);
    println!("[view] ---- 统计（{}s）----", opts.seconds.max(1));
    println!(
        "[view] DATAGRAM = {}（{}），重组帧 = {}，解码帧 = {}，关键帧 = {}，解析失败 = {}",
        stats.datagrams,
        dfy_transport::human_bytes(stats.bytes),
        stats.frames,
        stats.decoded,
        stats.keyframes,
        stats.malformed
    );
    if stats.audio_packets > 0 || opts.audio_in {
        println!(
            "[view] 音频：DATAGRAM = {}，播放帧 = {}，最后序号 = {}，丢包 = {}，淘汰 = {}，乱序 = {}，重复 = {}，峰值深度 = {}",
            stats.audio_packets,
            stats.audio_frames,
            stats.audio_last_seq,
            stats.audio_lost,
            stats.audio_evicted,
            stats.audio_reordered,
            stats.audio_duplicates,
            stats.audio_depth
        );
    }
    println!(
        "[view] RTT = {:?} ms，结束原因 = {}，质量反馈窗口 = {}，收尾解码 = {}",
        stats.rtt_ms, stats.end_reason, stats.feedback_windows_sent, stats.drained_frames
    );
    println!(
        "[view] 路径迁移：次数 = {}，恢复确认 = {}，最后帧 = #{}/{}",
        stats.path_switches, stats.resume_ok, stats.last_frame_id, stats.last_frame_seq
    );
    if opts.control && control_allowed {
        println!(
            "[view] 控制输入：鼠标移动 DATAGRAM = {}，其它事件 = {}",
            stats.input_moves, stats.input_events
        );
    }
    if let Some(sync) = clipboard.as_ref() {
        let c = sync.stats();
        println!(
            "[view] 剪贴板：清单 {}↑/{}↓，拉取 {}↑/{}↓，分块 {}↑/{}↓，确认 {}↑/{}↓，字节 {}↑/{}↓",
            c.lists_sent,
            c.lists_received,
            c.pulls_sent,
            c.pulls_received,
            c.chunks_sent,
            c.chunks_received,
            c.applied_sent,
            c.applied_received,
            dfy_transport::human_bytes(c.bytes_sent),
            dfy_transport::human_bytes(c.bytes_received)
        );
    }
    if let Some(view_files) = files.as_ref() {
        let f = view_files.stats();
        if !view_files.is_done() {
            println!("[view] 注意：文件队列尚未发送完成（对端提前结束会话？）");
        }
        println!(
            "[view] 文件：offer {}，块 {}（{}），确认 {}，结果 {}，完成 {}，失败 {}",
            f.offers_sent,
            f.chunks_sent,
            dfy_transport::human_bytes(f.bytes),
            f.acks_received,
            f.results_received,
            f.files_completed,
            f.files_failed
        );
    }
    info!(
        datagrams = stats.datagrams,
        frames = stats.frames,
        decoded = stats.decoded,
        "view 结束"
    );
    Ok(stats)
}

/// 便于测试：把一帧 BGRA 写成 PPM（P6）以便无 GUI 环境下人工查看。
#[allow(dead_code)]
pub fn bgra_to_ppm(frame: &dfy_session::DecodedFrame) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + frame.data.len() / 4 * 3);
    out.extend_from_slice(format!("P6\n{} {}\n255\n", frame.width, frame.height).as_bytes());
    for px in frame.data.chunks_exact(4) {
        out.push(px[2]);
        out.push(px[1]);
        out.push(px[0]);
    }
    out
}

/// 便于其他模块构造原始帧（测试用）。
#[allow(dead_code)]
pub fn blank_frame(width: u32, height: u32) -> RawFrame {
    RawFrame {
        width,
        height,
        stride: width * 4,
        data: vec![0u8; (width * height * 4) as usize],
        pts_us: 0,
        full_frame: true,
    }
}

/// 解析指纹文本的小工具（供 main.rs 复用）。
pub fn parse_fingerprint_arg(text: &str) -> Result<[u8; 32]> {
    parse_fingerprint_hex(text).context("解析证书指纹失败（需要 64 位 hex）")
}

/// 构造一条键盘输入事件（HID Usage + 修饰键位图）。
pub(crate) fn input_key_envelope(usage: u16, down: bool, modifiers: u8) -> dfy_proto::Envelope {
    input_envelope(
        INPUT_KIND_KEY,
        u32::from(usage),
        u32::from(modifiers),
        if down {
            INPUT_STATE_DOWN
        } else {
            INPUT_STATE_UP
        },
        0,
        0,
    )
}

/// 构造一条鼠标按键输入事件（1=左、2=右、3=中、4=后退、5=前进）。
pub(crate) fn input_button_envelope(button: u8, down: bool) -> dfy_proto::Envelope {
    input_envelope(
        INPUT_KIND_MOUSE_BTN,
        u32::from(button),
        0,
        if down {
            INPUT_STATE_DOWN
        } else {
            INPUT_STATE_UP
        },
        0,
        0,
    )
}

/// 构造一条滚轮输入事件（单位：WHEEL_DELTA=120）。
pub(crate) fn input_wheel_envelope(dx: i16, dy: i16) -> dfy_proto::Envelope {
    input_envelope(
        INPUT_KIND_WHEEL,
        0,
        0,
        INPUT_STATE_DOWN,
        i32::from(dx),
        i32::from(dy),
    )
}

fn input_envelope(
    kind: u32,
    hid_usage: u32,
    mods: u32,
    state: u32,
    dx: i32,
    dy: i32,
) -> dfy_proto::Envelope {
    dfy_proto::Envelope {
        body: Some(envelope::Body::InputEvent(ProtoInputEvent {
            kind,
            hid_usage,
            mods,
            state,
            x: 0,
            y: 0,
            dx,
            dy,
            unicode: 0,
            ts_us: dfy_transport::now_us(),
        })),
    }
}

/// 窗口内画面坐标 → 0..65535 归一化绝对坐标（协议 absolute move 用 65536 虚拟尺寸）。
pub(crate) fn frame_point_to_move(
    x: u32,
    y: u32,
    width: u32,
    height: u32,
) -> Option<ProtoInputEvent> {
    if width < 2 || height < 2 {
        return None;
    }
    let nx = (u64::from(x.min(width - 1)) * 65_535 / (u64::from(width) - 1)) as u32;
    let ny = (u64::from(y.min(height - 1)) * 65_535 / (u64::from(height) - 1)) as u32;
    Some(ProtoInputEvent {
        kind: INPUT_KIND_MOUSE_MOVE,
        hid_usage: 0,
        mods: 0,
        state: INPUT_STATE_DOWN,
        x: nx,
        y: ny,
        dx: 0,
        dy: 0,
        unicode: 0,
        ts_us: dfy_transport::now_us(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loss_ppm_from_quic_deltas() {
        assert_eq!(loss_ppm_from_deltas(0, 0), 0);
        assert_eq!(loss_ppm_from_deltas(0, 1_000), 0);
        assert_eq!(loss_ppm_from_deltas(1, 999), 1_000);
        assert_eq!(loss_ppm_from_deltas(2, 8), 200_000);
        // 溢出与饱和：大增量不产生 panic；ppm 上限恒为 1_000_000。
        assert_eq!(loss_ppm_from_deltas(1_000_000, 0), 1_000_000);
        assert_eq!(loss_ppm_from_deltas(u64::MAX, u64::MAX), 1);
    }

    #[test]
    fn bgra_to_ppm_header() {
        let frame = dfy_session::DecodedFrame {
            width: 2,
            height: 1,
            stride: 8,
            data: vec![1, 2, 3, 255, 4, 5, 6, 255],
            pts_us: 0,
        };
        let ppm = bgra_to_ppm(&frame);
        assert!(ppm.starts_with(b"P6\n2 1\n255\n"));
        assert_eq!(ppm.len(), 11 + 6);
        assert_eq!(&ppm[11..], &[3, 2, 1, 6, 5, 4]);
    }

    #[test]
    fn quality_control_options_build_request_and_reset() {
        assert!(quality_control_envelope(false, None, None, None, None)
            .expect("no quality request")
            .is_none());
        assert_eq!(
            quality_control_envelope(false, Some(1920), Some(1080), Some(120), Some(100_000))
                .expect("valid maximum quality request"),
            Some(dfy_proto::Envelope::quality_control(
                1920, 1080, 120, 100_000
            ))
        );
        assert_eq!(
            quality_control_envelope(false, Some(2), Some(2), Some(1), Some(100))
                .expect("valid minimum quality request"),
            Some(dfy_proto::Envelope::quality_control(2, 2, 1, 100))
        );
        assert_eq!(
            quality_control_envelope(true, None, None, None, None).expect("valid quality reset"),
            Some(dfy_proto::Envelope::quality_control(0, 0, 0, 0))
        );
    }

    #[test]
    fn quality_control_options_require_all_four_request_fields() {
        for (width, height, fps, bitrate) in [
            (Some(1920), None, None, None),
            (None, Some(1080), None, None),
            (None, None, Some(60), None),
            (None, None, None, Some(8000)),
            (Some(1920), Some(1080), Some(60), None),
        ] {
            let error = quality_control_envelope(false, width, height, fps, bitrate)
                .expect_err("incomplete quality request must fail");
            let message = error.to_string();
            for option in [
                "--quality-width",
                "--quality-height",
                "--quality-fps",
                "--quality-bitrate",
            ] {
                assert!(message.contains(option), "{message}");
            }
        }
    }

    #[test]
    fn quality_control_options_validate_ranges_and_even_dimensions() {
        for (width, height, fps, bitrate, expected) in [
            (
                0,
                1080,
                60,
                8000,
                "--quality-width/--quality-height 必须大于 0",
            ),
            (
                1920,
                0,
                60,
                8000,
                "--quality-width/--quality-height 必须大于 0",
            ),
            (1919, 1080, 60, 8000, "必须为偶数"),
            (1920, 1081, 60, 8000, "必须为偶数"),
            (1920, 1080, 0, 8000, "--quality-fps 必须在 1..=120"),
            (1920, 1080, 121, 8000, "--quality-fps 必须在 1..=120"),
            (1920, 1080, 60, 99, "--quality-bitrate 必须在 100..=100000"),
            (
                1920,
                1080,
                60,
                100_001,
                "--quality-bitrate 必须在 100..=100000",
            ),
        ] {
            let error = quality_control_envelope(
                false,
                Some(width),
                Some(height),
                Some(fps),
                Some(bitrate),
            )
            .expect_err("out-of-range quality request must fail");
            assert!(error.to_string().contains(expected), "{error}");
        }
    }

    #[test]
    fn quality_control_options_reject_reset_with_request_fields() {
        let error = quality_control_envelope(true, Some(1920), None, None, None)
            .expect_err("reset and request must not be combined");
        assert!(error.to_string().contains("--reset-quality"));
    }
}
