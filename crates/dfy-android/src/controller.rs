// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! Android controller/client loop. Video stays as H.264 Annex-B and is handed to
//! Kotlin for MediaCodec/Surface presentation; no desktop-only decoder is loaded.

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use dfy_proto::consts::{
    INPUT_KIND_KEY, INPUT_KIND_MOUSE_BTN, INPUT_KIND_MOUSE_MOVE, INPUT_KIND_WHEEL,
    INPUT_STATE_DOWN, INPUT_STATE_UP, PERM_CONTROL, PERM_SYSTEM_ACTION, PERM_VIEW,
};
use dfy_proto::{decode_video, envelope, Envelope, InputEvent, QualityControl, StreamFeedback};
use dfy_session::{EncodeResolution, FrameAssembler, RuntimeSettings};
use dfy_transport::{
    client_handshake, client_tls, connect, parse_fingerprint_hex, send_input_move, CtrlChannel,
    Heartbeat,
};
use quinn::Connection;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;
use zeroize::Zeroizing;

use crate::agent::MAX_ENCODED_FRAME_BYTES;

/// Additive host opt-in. A host must also grant PERM_CONTROL | PERM_SYSTEM_ACTION.
/// The events use ordinary HID Escape/Home so existing input backends need no new wire type.
pub const SYSTEM_ACTION_CAPABILITY: &str = "system-action.back-home.v1";
pub const STREAM_FEEDBACK_CAPABILITY: &str = "stream-feedback.v1";
pub const QUALITY_CONTROL_CAPABILITY: &str = "quality-control.v1";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const INPUT_QUEUE_CAPACITY: usize = 256;

pub struct ControllerSettings {
    pub host: String,
    pub port: u16,
    pub password: Zeroizing<String>,
    pub fingerprint: [u8; 32],
}

impl ControllerSettings {
    pub fn new(host: String, port: u16, password: String, fingerprint: &str) -> Result<Self> {
        anyhow::ensure!(!host.trim().is_empty(), "主机地址不能为空");
        anyhow::ensure!(
            host.len() <= 253 && !host.chars().any(char::is_whitespace),
            "主机地址无效"
        );
        anyhow::ensure!(port != 0, "端口必须在 1–65535 之间");
        anyhow::ensure!(
            (6..=1024).contains(&password.len()) && !password.trim().is_empty(),
            "会话口令至少 6 位（6–1024 字节），且不能全为空白"
        );
        anyhow::ensure!(
            fingerprint
                .chars()
                .all(|c| c.is_ascii_hexdigit() || matches!(c, ':' | '-' | ' ' | '\t' | '\r' | '\n')),
            "证书指纹只能包含十六进制字符和分隔符"
        );
        let fingerprint = parse_fingerprint_hex(fingerprint)?;
        Ok(Self {
            host,
            port,
            password: Zeroizing::new(password),
            fingerprint,
        })
    }

    /// Strict fingerprint text used for display, never a trust-on-first-use value.
    pub fn fingerprint_hex(&self) -> String {
        self.fingerprint
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

pub trait ControllerCallbacks: Send + Sync {
    fn on_state(&self, state: &str, detail: &str);
    fn on_frame(&self, data: &[u8], pts_us: u32, keyframe: bool) -> bool;
    fn on_video_packet(
        &self,
        datagram_bytes: usize,
        frames_assembled: u32,
        frames_delivered: u32,
        dropped_frames: u64,
    );
    fn on_video_config(&self, width: u32, height: u32, fps: u32, bitrate_kbps: u32);
    fn on_session(&self, permissions: u32, capabilities: &[String]);
    fn on_control_allowed(&self, allowed: bool);
}

#[derive(Debug, Default)]
pub struct ControllerStats {
    pub packets_recv: AtomicU64,
    pub bytes_recv: AtomicU64,
    pub frames_assembled: AtomicU64,
    pub frames_submitted: AtomicU64,
    pub key_packets: AtomicU64,
    pub malformed: AtomicU64,
    pub dropped_frames: AtomicU64,
    pub input_moves: AtomicU64,
    pub input_events: AtomicU64,
    pub rejected_input: AtomicU64,
    pub quality_controls: AtomicU64,
    pub quality_control_errors: AtomicU64,
    pub rtt_ms: AtomicU64,
}

impl ControllerStats {
    pub fn reset(&self) {
        for value in [
            &self.packets_recv,
            &self.bytes_recv,
            &self.frames_assembled,
            &self.frames_submitted,
            &self.key_packets,
            &self.malformed,
            &self.dropped_frames,
            &self.input_moves,
            &self.input_events,
            &self.rejected_input,
            &self.quality_controls,
            &self.quality_control_errors,
            &self.rtt_ms,
        ] {
            value.store(0, Ordering::Relaxed);
        }
    }

    pub fn snapshot(&self) -> String {
        format!(
            "{{\"packets_recv\":{},\"bytes_recv\":{},\"frames_assembled\":{},\"frames_submitted\":{},\"key_packets\":{},\"malformed\":{},\"dropped_frames\":{},\"input_moves\":{},\"input_events\":{},\"rejected_input\":{},\"quality_controls\":{},\"quality_control_errors\":{},\"rtt_ms\":{}}}",
            self.packets_recv.load(Ordering::Relaxed),
            self.bytes_recv.load(Ordering::Relaxed),
            self.frames_assembled.load(Ordering::Relaxed),
            self.frames_submitted.load(Ordering::Relaxed),
            self.key_packets.load(Ordering::Relaxed),
            self.malformed.load(Ordering::Relaxed),
            self.dropped_frames.load(Ordering::Relaxed),
            self.input_moves.load(Ordering::Relaxed),
            self.input_events.load(Ordering::Relaxed),
            self.rejected_input.load(Ordering::Relaxed),
            self.quality_controls.load(Ordering::Relaxed),
            self.quality_control_errors.load(Ordering::Relaxed),
            self.rtt_ms.load(Ordering::Relaxed),
        )
    }
}

#[derive(Debug)]
pub struct ControllerAuthorization {
    local: AtomicBool,
    permissions: AtomicU32,
    move_datagram: AtomicBool,
    system_actions: AtomicBool,
    stream_feedback: AtomicBool,
    quality_control: AtomicBool,
}

impl Default for ControllerAuthorization {
    fn default() -> Self {
        Self {
            local: AtomicBool::new(false),
            permissions: AtomicU32::new(0),
            move_datagram: AtomicBool::new(false),
            system_actions: AtomicBool::new(false),
            stream_feedback: AtomicBool::new(false),
            quality_control: AtomicBool::new(false),
        }
    }
}

impl ControllerAuthorization {
    pub fn set_local(&self, allowed: bool) {
        self.local.store(allowed, Ordering::Release);
    }

    pub fn set_session(&self, permissions: u32, capabilities: &[String]) {
        self.permissions.store(permissions, Ordering::Release);
        self.move_datagram.store(
            capabilities.iter().any(|c| c == "input-move.datagram.v1"),
            Ordering::Release,
        );
        self.system_actions.store(
            capabilities.iter().any(|c| c == SYSTEM_ACTION_CAPABILITY),
            Ordering::Release,
        );
        self.stream_feedback.store(
            capabilities.iter().any(|c| c == STREAM_FEEDBACK_CAPABILITY),
            Ordering::Release,
        );
        self.quality_control.store(
            capabilities.iter().any(|c| c == QUALITY_CONTROL_CAPABILITY),
            Ordering::Release,
        );
    }

    pub fn clear_session(&self) {
        self.permissions.store(0, Ordering::Release);
        self.move_datagram.store(false, Ordering::Release);
        self.system_actions.store(false, Ordering::Release);
        self.stream_feedback.store(false, Ordering::Release);
        self.quality_control.store(false, Ordering::Release);
    }

    pub fn control_allowed(&self) -> bool {
        self.local.load(Ordering::Acquire)
            && self.permissions.load(Ordering::Acquire) & PERM_CONTROL != 0
    }

    pub fn move_datagram_allowed(&self) -> bool {
        self.control_allowed() && self.move_datagram.load(Ordering::Acquire)
    }

    pub fn stream_feedback_allowed(&self) -> bool {
        self.stream_feedback.load(Ordering::Acquire)
    }

    pub fn quality_control_allowed(&self) -> bool {
        self.permissions.load(Ordering::Acquire) & PERM_CONTROL != 0
            && self.quality_control.load(Ordering::Acquire)
    }

    pub fn system_action_allowed(&self) -> bool {
        self.control_allowed()
            && self.system_actions.load(Ordering::Acquire)
            && self.permissions.load(Ordering::Acquire) & PERM_SYSTEM_ACTION != 0
    }
}

pub enum ControllerCommand {
    Pointer(InputEvent),
    Button(InputEvent),
    Wheel(InputEvent),
    Key(InputEvent),
    SystemAction(SystemAction),
    ControlAllowed(bool),
    ReleaseInput,
    RequestKeyframe,
    StreamFeedback(StreamFeedback),
    QualityControl(QualityControl),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SystemAction {
    Back,
    Home,
}

pub fn command_channel() -> (
    mpsc::Sender<ControllerCommand>,
    mpsc::Receiver<ControllerCommand>,
) {
    mpsc::channel(INPUT_QUEUE_CAPACITY)
}

pub fn quality_control(
    width: i64,
    height: i64,
    fps: i64,
    bitrate_kbps: i64,
) -> Result<QualityControl> {
    let width = u32::try_from(width).context("画质宽度必须为非负整数")?;
    let height = u32::try_from(height).context("画质高度必须为非负整数")?;
    let fps = u32::try_from(fps).context("画质帧率必须为非负整数")?;
    let bitrate_kbps = u32::try_from(bitrate_kbps).context("画质码率必须为非负整数")?;
    if width == 0 && height == 0 && fps == 0 && bitrate_kbps == 0 {
        return Ok(QualityControl {
            width,
            height,
            fps,
            bitrate_kbps,
        });
    }
    RuntimeSettings {
        fps,
        bitrate_kbps,
        resolution: Some(EncodeResolution { width, height }),
    }
    .validate()?;
    Ok(QualityControl {
        width,
        height,
        fps,
        bitrate_kbps,
    })
}

pub fn pointer_event(x: u32, y: u32) -> InputEvent {
    InputEvent {
        kind: INPUT_KIND_MOUSE_MOVE,
        x: x.min(65_535),
        y: y.min(65_535),
        state: INPUT_STATE_DOWN,
        ..InputEvent::default()
    }
}

pub fn button_event(button: u32, down: bool, x: u32, y: u32) -> Result<InputEvent> {
    anyhow::ensure!((1..=5).contains(&button), "鼠标按键必须为 1–5");
    Ok(InputEvent {
        kind: INPUT_KIND_MOUSE_BTN,
        hid_usage: button,
        x: x.min(65_535),
        y: y.min(65_535),
        state: if down {
            INPUT_STATE_DOWN
        } else {
            INPUT_STATE_UP
        },
        ..InputEvent::default()
    })
}

pub fn wheel_event(dx: i32, dy: i32) -> InputEvent {
    InputEvent {
        kind: INPUT_KIND_WHEEL,
        dx,
        dy,
        ..InputEvent::default()
    }
}

pub fn key_event(usage: u32, down: bool, modifiers: u32) -> Result<InputEvent> {
    anyhow::ensure!(usage != 0 && usage <= 0xE7, "HID Usage 无效");
    Ok(InputEvent {
        kind: INPUT_KIND_KEY,
        hid_usage: usage,
        mods: modifiers & 0xFF,
        state: if down {
            INPUT_STATE_DOWN
        } else {
            INPUT_STATE_UP
        },
        ..InputEvent::default()
    })
}

pub fn stream_feedback(
    window_ms: i64,
    payload_bytes: i64,
    packets: i64,
    frames_assembled: i64,
    frames_delivered: i64,
    dropped_frames: i64,
    stall_ms: i64,
) -> Result<StreamFeedback> {
    let non_negative = |value: i64, name: &str| {
        anyhow::ensure!(value >= 0, "{name}不能为负数");
        Ok::<u64, anyhow::Error>(value as u64)
    };
    let window_ms = non_negative(window_ms, "反馈窗口")?;
    let payload_bytes = non_negative(payload_bytes, "负载字节")?;
    let packets = non_negative(packets, "数据包数")?;
    let frames_assembled = non_negative(frames_assembled, "重组帧数")?;
    let frames_delivered = non_negative(frames_delivered, "交付帧数")?;
    let dropped_frames = non_negative(dropped_frames, "丢帧数")?;
    let stall_ms = non_negative(stall_ms, "停顿时长")?;
    anyhow::ensure!(
        (1..=60_000).contains(&window_ms),
        "反馈窗口必须在 1–60000 毫秒之间"
    );
    anyhow::ensure!(stall_ms <= 60_000, "停顿时长不能超过 60000 毫秒");
    anyhow::ensure!(packets > 0 || dropped_frames > 0, "空闲窗口不能发送反馈");
    anyhow::ensure!(
        frames_delivered <= frames_assembled,
        "交付帧数不能超过重组帧数"
    );
    Ok(StreamFeedback {
        window_ms,
        payload_bytes,
        packets,
        frames_assembled,
        frames_delivered,
        dropped_frames,
        stall_ms,
    })
}

async fn resolve(settings: &ControllerSettings) -> Result<SocketAddr> {
    let literal = settings.host.parse::<IpAddr>().ok();
    if let Some(ip) = literal {
        return Ok(SocketAddr::new(ip, settings.port));
    }
    let mut addresses = tokio::net::lookup_host((settings.host.as_str(), settings.port))
        .await
        .context("解析主机地址失败")?;
    addresses.next().context("主机地址没有解析结果")
}

async fn connect_session(
    settings: &ControllerSettings,
) -> Result<(quinn::Endpoint, Connection, CtrlChannel, dfy_proto::Welcome)> {
    let address = resolve(settings).await?;
    let tls = client_tls(Some(settings.fingerprint))?;
    let (endpoint, conn) = connect(address, tls.clone()).await?;
    let observed = tls.observed_hex().context("TLS 未提供证书指纹")?;
    anyhow::ensure!(
        observed == settings.fingerprint_hex(),
        "证书指纹不匹配：期望 {}，实际 {}",
        settings.fingerprint_hex(),
        observed
    );
    let (welcome, chan) = client_handshake(
        &conn,
        settings.password.as_str(),
        "dfy-android-view",
        Vec::new(),
    )
    .await?;
    anyhow::ensure!(
        welcome.permission_bitmap & PERM_VIEW != 0,
        "Agent 未授予查看权限"
    );
    Ok((endpoint, conn, chan, welcome))
}

async fn release_held_input(
    tx: &mut dfy_transport::CtrlSender,
    held_keys: &mut HashSet<u16>,
    held_buttons: &mut HashSet<u8>,
) {
    for usage in held_keys.drain() {
        let event = match key_event(u32::from(usage), false, 0) {
            Ok(event) => event,
            Err(_) => continue,
        };
        let _ = tx.send_ctrl(&input_envelope(event)).await;
    }
    for button in held_buttons.drain() {
        let event = match button_event(u32::from(button), false, 0, 0) {
            Ok(event) => event,
            Err(_) => continue,
        };
        let _ = tx.send_ctrl(&input_envelope(event)).await;
    }
}

fn input_envelope(event: InputEvent) -> Envelope {
    Envelope {
        body: Some(envelope::Body::InputEvent(event)),
    }
}

async fn send_system_action(
    tx: &mut dfy_transport::CtrlSender,
    action: SystemAction,
) -> Result<()> {
    let usage = match action {
        SystemAction::Back => 0x29,
        SystemAction::Home => 0x4A,
    };
    for down in [true, false] {
        tx.send_ctrl(&input_envelope(key_event(usage, down, 0)?))
            .await?;
    }
    Ok(())
}

/// Small client-specific loop. `dfy_session::run_client` cannot be reused because it
/// requires a CPU BGRA Decoder and does not expose the control sender to Android input.
pub async fn run_controller(
    settings: ControllerSettings,
    callbacks: Arc<dyn ControllerCallbacks>,
    stats: Arc<ControllerStats>,
    authorization: Arc<ControllerAuthorization>,
    mut commands: mpsc::Receiver<ControllerCommand>,
    shutdown: Arc<tokio::sync::Notify>,
) -> Result<String> {
    let shutdown_owner = Arc::clone(&shutdown);
    let mut shutdown_signal = Box::pin(shutdown_owner.notified());
    callbacks.on_state(
        "connecting",
        &format!("{}:{}", settings.host, settings.port),
    );
    let opened = tokio::time::timeout(CONNECT_TIMEOUT, connect_session(&settings)).await;
    let (endpoint, conn, chan, welcome) = match opened {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => {
            authorization.clear_session();
            callbacks.on_state("error", &format!("{error:#}"));
            return Err(error);
        }
        Err(_) => {
            authorization.clear_session();
            let error = anyhow::anyhow!("连接或认证超时（{}s）", CONNECT_TIMEOUT.as_secs());
            callbacks.on_state("error", &error.to_string());
            return Err(error);
        }
    };
    authorization.set_session(welcome.permission_bitmap, &welcome.capability);
    callbacks.on_session(welcome.permission_bitmap, &welcome.capability);
    callbacks.on_control_allowed(authorization.control_allowed());
    callbacks.on_state("connected", &welcome.agent_name);
    let feedback_enabled = welcome
        .capability
        .iter()
        .any(|name| name == STREAM_FEEDBACK_CAPABILITY);

    let (mut tx, mut rx) = chan.split();
    let mut heartbeat = Heartbeat::new();
    let mut assembler = FrameAssembler::with_max_bytes(MAX_ENCODED_FRAME_BYTES);
    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut held_keys = HashSet::new();
    let mut held_buttons = HashSet::new();
    let mut oversized_frames = 0u64;
    let mut end_reason = "stopped";

    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown_signal => break,
            _ = ticker.tick() => {
                if Instant::now() >= heartbeat.deadline() {
                    let ts = dfy_transport::now_us();
                    heartbeat.mark_ping_sent(ts);
                    if let Err(error) = tx.send_ctrl(&Envelope::ping(ts)).await {
                        tracing::debug!(%error, "Android controller: Ping failed");
                        end_reason = "ctrl_closed";
                        break;
                    }
                }
                if heartbeat.timed_out() {
                    end_reason = "heartbeat_timeout";
                    break;
                }
                stats.rtt_ms.store(
                    heartbeat.rtt_ms().map_or(0, |ms| ms.max(0.0) as u64),
                    Ordering::Relaxed,
                );
            }
            incoming = rx.recv_ctrl() => {
                match incoming {
                    Ok(None) => { end_reason = "peer_closed"; break; }
                    Err(error) => {
                        tracing::debug!(%error, "Android controller: control receive failed");
                        end_reason = "ctrl_closed";
                        break;
                    }
                    Ok(Some(env)) => match env.body {
                        Some(envelope::Body::Ping(ping)) => {
                            if let Err(error) = tx.send_ctrl(&Envelope::pong(ping.ts_us)).await {
                                tracing::debug!(%error, "Android controller: Pong failed");
                            }
                        }
                        Some(envelope::Body::Pong(_)) => { heartbeat.on_frame(&env); }
                        Some(envelope::Body::Bye(bye)) => {
                            end_reason = if bye.reason == dfy_proto::BYE_NORMAL { "agent_bye" } else { "agent_bye_user" };
                            break;
                        }
                        Some(envelope::Body::Reject(reject)) => {
                            end_reason = "rejected";
                            callbacks.on_state("error", &format!("Agent 拒绝会话：{}", dfy_proto::error_name(reject.code)));
                            break;
                        }
                        Some(envelope::Body::VideoConfig(config))
                            if config.width != 0 && config.height != 0 =>
                        {
                            callbacks.on_video_config(config.width, config.height, config.fps, config.bitrate_kbps);
                        }
                        _ => {}
                    }
                }
            }
            datagram = conn.read_datagram() => {
                match datagram {
                    Ok(bytes) => {
                        let (header, payload) = match decode_video(&bytes) {
                            Ok(value) => value,
                            Err(_) => { stats.malformed.fetch_add(1, Ordering::Relaxed); continue; }
                        };
                        stats.packets_recv.fetch_add(1, Ordering::Relaxed);
                        stats.bytes_recv.fetch_add(payload.len() as u64, Ordering::Relaxed);
                        if header.is_key() { stats.key_packets.fetch_add(1, Ordering::Relaxed); }
                        let dropped_before = assembler.dropped();
                        let mut assembled_delta = 0u32;
                        let mut delivered_delta = 0u32;
                        let mut oversized_delta = 0u64;
                        match assembler.push(&header, payload) {
                            Ok(Some(frame)) => {
                                assembled_delta = 1;
                                stats.frames_assembled.fetch_add(1, Ordering::Relaxed);
                                if frame.data.len() > MAX_ENCODED_FRAME_BYTES {
                                    oversized_frames = oversized_frames.saturating_add(1);
                                    oversized_delta = 1;
                                } else {
                                    if callbacks.on_frame(&frame.data, frame.ts_us, frame.keyframe) {
                                        delivered_delta = 1;
                                    }
                                    stats.frames_submitted.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                            Ok(None) => {}
                            Err(error) => tracing::debug!(%error, "Android controller: reassembly failed"),
                        }
                        let dropped_delta = assembler.dropped().saturating_sub(dropped_before)
                            .saturating_add(oversized_delta);
                        if feedback_enabled {
                            callbacks.on_video_packet(
                                bytes.len(),
                                assembled_delta,
                                delivered_delta,
                                dropped_delta,
                            );
                        }
                        stats.dropped_frames.store(
                            assembler.dropped().saturating_add(oversized_frames),
                            Ordering::Relaxed,
                        );
                    }
                    Err(error) => {
                        tracing::debug!(%error, "Android controller: DATAGRAM receive failed");
                        end_reason = "conn_closed";
                        break;
                    }
                }
            }
            command = commands.recv() => {
                let Some(command) = command else { end_reason = "stopped"; break; };
                match command {
                    ControllerCommand::ControlAllowed(allowed) => {
                        authorization.set_local(allowed);
                        if !allowed { release_held_input(&mut tx, &mut held_keys, &mut held_buttons).await; }
                        callbacks.on_control_allowed(authorization.control_allowed());
                    }
                    ControllerCommand::ReleaseInput => release_held_input(&mut tx, &mut held_keys, &mut held_buttons).await,
                    ControllerCommand::RequestKeyframe => {
                        let _ = tx.send_ctrl(&Envelope::keyframe_request()).await;
                    }
                    ControllerCommand::StreamFeedback(feedback) => {
                        if authorization.stream_feedback_allowed() {
                            if let Err(error) = tx.send_ctrl(&Envelope::stream_feedback(feedback)).await {
                                tracing::debug!(%error, "Android controller: StreamFeedback failed");
                                end_reason = "ctrl_closed";
                                break;
                            }
                        }
                    }
                    ControllerCommand::QualityControl(request) => {
                        if authorization.quality_control_allowed() {
                            if tx.send_ctrl(&Envelope::quality_control(
                                request.width,
                                request.height,
                                request.fps,
                                request.bitrate_kbps,
                            )).await.is_ok() {
                                stats.quality_controls.fetch_add(1, Ordering::Relaxed);
                            } else {
                                stats.quality_control_errors.fetch_add(1, Ordering::Relaxed);
                                end_reason = "ctrl_closed";
                                break;
                            }
                        } else {
                            stats.quality_control_errors.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    ControllerCommand::Pointer(event) if authorization.move_datagram_allowed() => {
                        if let Err(error) = send_input_move(&conn, &event) {
                            tracing::debug!(%error, "Android controller: pointer DATAGRAM failed");
                            stats.rejected_input.fetch_add(1, Ordering::Relaxed);
                        } else { stats.input_moves.fetch_add(1, Ordering::Relaxed); }
                    }
                    ControllerCommand::Pointer(event) if authorization.control_allowed() => {
                        if tx.send_ctrl(&input_envelope(event)).await.is_ok() { stats.input_moves.fetch_add(1, Ordering::Relaxed); }
                    }
                    ControllerCommand::Button(event) if authorization.control_allowed() => {
                        if event.state == INPUT_STATE_DOWN { held_buttons.insert(event.hid_usage as u8); }
                        else { held_buttons.remove(&(event.hid_usage as u8)); }
                        if tx.send_ctrl(&input_envelope(event)).await.is_ok() { stats.input_events.fetch_add(1, Ordering::Relaxed); }
                    }
                    ControllerCommand::Wheel(event) if authorization.control_allowed() => {
                        if tx.send_ctrl(&input_envelope(event)).await.is_ok() { stats.input_events.fetch_add(1, Ordering::Relaxed); }
                    }
                    ControllerCommand::Key(event) if authorization.control_allowed() => {
                        if event.state == INPUT_STATE_DOWN { held_keys.insert(event.hid_usage as u16); }
                        else { held_keys.remove(&(event.hid_usage as u16)); }
                        if tx.send_ctrl(&input_envelope(event)).await.is_ok() { stats.input_events.fetch_add(1, Ordering::Relaxed); }
                    }
                    ControllerCommand::SystemAction(action) if authorization.system_action_allowed() => {
                        if send_system_action(&mut tx, action).await.is_ok() { stats.input_events.fetch_add(2, Ordering::Relaxed); }
                    }
                    _ => { stats.rejected_input.fetch_add(1, Ordering::Relaxed); }
                }
            }
        }
    }

    // Releases are attempted even after local authorization is revoked.
    let _ = tokio::time::timeout(
        Duration::from_millis(500),
        release_held_input(&mut tx, &mut held_keys, &mut held_buttons),
    )
    .await;
    let _ = tx.send_ctrl(&Envelope::bye(dfy_proto::BYE_NORMAL)).await;
    let _ = tx.finish();
    conn.close(0u32.into(), b"android viewer stopped");
    endpoint.close(0u32.into(), b"android viewer stopped");
    authorization.clear_session();
    stats.dropped_frames.store(
        assembler.dropped().saturating_add(oversized_frames),
        Ordering::Relaxed,
    );
    callbacks.on_control_allowed(false);
    callbacks.on_state("disconnected", end_reason);
    Ok(end_reason.to_string())
}

/// Used by JNI shutdown before taking the runtime; cancellation-safe and idempotent.
pub fn notify_shutdown(shutdown: &Arc<tokio::sync::Notify>) {
    shutdown.notify_waiters();
}

#[cfg(test)]
mod tests {
    use super::*;
    use dfy_proto::{VideoHeader, CHAN_VIDEO, VIDEO_FLAG_KEY, VIDEO_FLAG_LAST};
    use std::sync::Mutex;

    #[test]
    fn settings_require_password_policy_and_strict_pin() {
        assert!(ControllerSettings::new(
            "127.0.0.1".into(),
            24443,
            "12345".into(),
            &"11".repeat(32)
        )
        .is_err());
        assert!(ControllerSettings::new(
            "127.0.0.1".into(),
            24443,
            "      ".into(),
            &"11".repeat(32)
        )
        .is_err());
        assert!(ControllerSettings::new(
            "127.0.0.1".into(),
            24443,
            "123456".into(),
            &"11".repeat(32)
        )
        .is_ok());
        assert!(ControllerSettings::new(
            "".into(),
            24443,
            "0123456789abcdef".into(),
            &"11".repeat(32)
        )
        .is_err());
        assert!(ControllerSettings::new(
            "127.0.0.1".into(),
            0,
            "0123456789abcdef".into(),
            &"11".repeat(32)
        )
        .is_err());
        assert!(ControllerSettings::new(
            "127.0.0.1".into(),
            24443,
            "0123456789abcdef".into(),
            "abc"
        )
        .is_err());
        let settings = ControllerSettings::new(
            "127.0.0.1".into(),
            24443,
            "0123456789abcdef".into(),
            &"AB:11".repeat(16),
        )
        .unwrap();
        assert_eq!(settings.fingerprint_hex(), "ab11".repeat(16));
        assert!(ControllerSettings::new(
            "127.0.0.1".into(),
            24443,
            "0123456789abcdef".into(),
            &("11".repeat(32) + "junk")
        )
        .is_err());
    }

    #[test]
    fn permissions_and_local_switch_are_both_enforced() {
        let auth = ControllerAuthorization::default();
        auth.set_session(PERM_VIEW | PERM_CONTROL, &[SYSTEM_ACTION_CAPABILITY.into()]);
        assert!(!auth.control_allowed());
        auth.set_local(true);
        assert!(auth.control_allowed());
        assert!(
            !auth.system_action_allowed(),
            "test grants control but not system permission"
        );
        auth.set_session(
            PERM_VIEW | PERM_CONTROL | PERM_SYSTEM_ACTION,
            &[
                SYSTEM_ACTION_CAPABILITY.into(),
                STREAM_FEEDBACK_CAPABILITY.into(),
            ],
        );
        assert!(auth.system_action_allowed());
        assert!(auth.stream_feedback_allowed());
        assert!(!auth.quality_control_allowed());
        auth.set_session(
            PERM_VIEW | PERM_CONTROL | PERM_SYSTEM_ACTION,
            &[
                SYSTEM_ACTION_CAPABILITY.into(),
                STREAM_FEEDBACK_CAPABILITY.into(),
                QUALITY_CONTROL_CAPABILITY.into(),
            ],
        );
        assert!(auth.quality_control_allowed());
        auth.set_session(PERM_VIEW | PERM_CONTROL | PERM_SYSTEM_ACTION, &[]);
        assert!(
            !auth.system_action_allowed(),
            "permission alone must not enable system actions"
        );
        assert!(!auth.stream_feedback_allowed());
        assert!(!auth.quality_control_allowed());
        auth.set_local(false);
        assert!(!auth.control_allowed());
    }

    #[test]
    fn input_builders_validate_and_clamp() {
        assert_eq!(pointer_event(70_000, 4).x, 65_535);
        assert!(button_event(0, true, 0, 0).is_err());
        assert!(button_event(6, true, 0, 0).is_err());
        assert!(key_event(0, true, 0).is_err());
        assert!(key_event(0xE8, true, 0).is_err());
        assert_eq!(key_event(0x29, true, 0x1FF).unwrap().mods, 0xFF);
    }

    #[test]
    fn quality_control_validates_fields_and_supports_reset() {
        let request = quality_control(3840, 2160, 120, 100_000).unwrap();
        assert_eq!(request.width, 3840);
        assert_eq!(request.fps, 120);
        let reset = quality_control(0, 0, 0, 0).unwrap();
        assert_eq!(reset.width, 0);
        assert!(quality_control(0, 0, 0, 0).is_ok());
        assert!(quality_control(1920, 0, 60, 8000).is_err());
        assert!(quality_control(1921, 1080, 60, 8000).is_err());
        assert!(quality_control(1920, 1080, 121, 8000).is_err());
        assert!(quality_control(1920, 1080, 60, 99).is_err());
        assert!(quality_control(-1, 1080, 60, 8000).is_err());
    }

    #[test]
    fn stream_feedback_validates_wire_bounds() {
        let feedback = stream_feedback(500, 1_000, 2, 1, 1, 0, 250).unwrap();
        assert_eq!(feedback.window_ms, 500);
        assert_eq!(feedback.stall_ms, 250);
        assert!(stream_feedback(0, 0, 1, 0, 0, 0, 0).is_err());
        assert!(stream_feedback(60_001, 0, 1, 0, 0, 0, 0).is_err());
        assert!(stream_feedback(500, -1, 1, 0, 0, 0, 0).is_err());
        assert!(stream_feedback(500, 0, 0, 0, 0, 0, 0).is_err());
        assert!(stream_feedback(500, 0, 1, 0, 1, 0, 0).is_err());
        assert!(stream_feedback(500, 0, 1, 0, 0, 0, 60_001).is_err());
        assert!(stream_feedback(500, 0, 0, 0, 0, 1, 60_000).is_ok());
    }

    #[derive(Default)]
    struct TestCallbacks {
        frames: Mutex<Vec<Vec<u8>>>,
        packets: Mutex<Vec<(usize, u32, u32, u64)>>,
        session: Mutex<Option<(u32, Vec<String>)>>,
    }
    impl ControllerCallbacks for TestCallbacks {
        fn on_state(&self, _state: &str, _detail: &str) {}
        fn on_frame(&self, data: &[u8], _pts_us: u32, _keyframe: bool) -> bool {
            self.frames.lock().unwrap().push(data.to_vec());
            true
        }
        fn on_video_packet(
            &self,
            datagram_bytes: usize,
            frames_assembled: u32,
            frames_delivered: u32,
            dropped_frames: u64,
        ) {
            self.packets.lock().unwrap().push((
                datagram_bytes,
                frames_assembled,
                frames_delivered,
                dropped_frames,
            ));
        }
        fn on_video_config(&self, _width: u32, _height: u32, _fps: u32, _bitrate_kbps: u32) {}
        fn on_session(&self, permissions: u32, capabilities: &[String]) {
            *self.session.lock().unwrap() = Some((permissions, capabilities.to_vec()));
        }
        fn on_control_allowed(&self, _allowed: bool) {}
    }

    #[tokio::test]
    async fn authenticated_client_reassembles_agent_datagrams() {
        let password = "0123456789abcdef";
        let (server_endpoint, identity, address) = dfy_transport::bind_agent_random().unwrap();
        let callbacks = Arc::new(TestCallbacks::default());
        let server_callbacks = callbacks.clone();
        let server = tokio::spawn(async move {
            let incoming = server_endpoint.accept().await.unwrap();
            let conn = incoming.await.unwrap();
            let mut auth = dfy_transport::ServerAuth::new(password, "test-agent");
            auth.permission_bitmap = PERM_VIEW | PERM_CONTROL;
            auth.capabilities = vec![STREAM_FEEDBACK_CAPABILITY.into()];
            let (_welcome, chan) = dfy_transport::server_handshake(&conn, &auth).await.unwrap();
            let (_tx, mut rx) = chan.split();
            let first = rx.recv_ctrl().await.unwrap().unwrap();
            assert!(
                matches!(first.body, Some(envelope::Body::KeyframeRequest(_))),
                "fresh Android session must not send legacy SessionResume: {first:?}"
            );
            let header = VideoHeader {
                chan: CHAN_VIDEO,
                flags: VIDEO_FLAG_KEY,
                frame_id: 1,
                frame_seq: 1,
                pkt_idx: 0,
                pkt_cnt: 2,
                ts_us: 123,
            };
            let first = dfy_proto::encode_video(&header, b"h264-").unwrap();
            conn.send_datagram(first.into()).unwrap();
            let mut second = header;
            second.pkt_idx = 1;
            second.flags = VIDEO_FLAG_LAST;
            let second = dfy_proto::encode_video(&second, b"annexb").unwrap();
            conn.send_datagram(second.into()).unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            let env = tokio::time::timeout(Duration::from_secs(2), rx.recv_ctrl())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let feedback = match env.body {
                Some(envelope::Body::StreamFeedback(feedback)) => feedback,
                other => panic!("expected StreamFeedback, got {other:?}"),
            };
            conn.close(0u32.into(), b"done");
            feedback
        });
        let settings = ControllerSettings::new(
            address.ip().to_string(),
            address.port(),
            password.into(),
            &identity.fingerprint_hex(),
        )
        .unwrap();
        let (tx, rx) = command_channel();
        let command_tx = tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            command_tx
                .send(ControllerCommand::RequestKeyframe)
                .await
                .unwrap();
            command_tx
                .send(ControllerCommand::StreamFeedback(
                    stream_feedback(500, 10, 2, 1, 1, 0, 0).unwrap(),
                ))
                .await
                .unwrap();
        });
        let reason = run_controller(
            settings,
            callbacks.clone(),
            Arc::new(ControllerStats::default()),
            Arc::new(ControllerAuthorization::default()),
            rx,
            Arc::new(tokio::sync::Notify::new()),
        )
        .await
        .unwrap();
        let feedback = server.await.unwrap();
        assert!(matches!(reason.as_str(), "peer_closed" | "ctrl_closed"));
        assert_eq!(feedback.packets, 2);
        assert_eq!(
            callbacks.frames.lock().unwrap().as_slice(),
            &[b"h264-annexb".to_vec()]
        );
        assert_eq!(
            callbacks.packets.lock().unwrap().as_slice(),
            &[(21, 0, 0, 0), (22, 1, 1, 0)]
        );
        assert_eq!(
            server_callbacks.session.lock().unwrap().as_ref().unwrap().0,
            PERM_VIEW | PERM_CONTROL
        );
    }

    #[tokio::test]
    async fn wrong_fingerprint_fails_before_authentication() {
        let (_server_endpoint, _identity, address) = dfy_transport::bind_agent_random().unwrap();
        let settings = ControllerSettings::new(
            address.ip().to_string(),
            address.port(),
            "0123456789abcdef".into(),
            &"11".repeat(32),
        )
        .unwrap();
        let callbacks = Arc::new(TestCallbacks::default());
        let (_tx, rx) = command_channel();
        let result = tokio::time::timeout(
            Duration::from_secs(20),
            run_controller(
                settings,
                callbacks.clone(),
                Arc::new(ControllerStats::default()),
                Arc::new(ControllerAuthorization::default()),
                rx,
                Arc::new(tokio::sync::Notify::new()),
            ),
        )
        .await
        .unwrap();
        assert!(result.is_err());
        assert!(callbacks.session.lock().unwrap().is_none());
    }
}
