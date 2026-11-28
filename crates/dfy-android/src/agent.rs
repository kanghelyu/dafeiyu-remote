// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! Android controlled-end callbacks and DFYP/1 agent loop.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Notify, Semaphore};

use anyhow::{Context, Result};
use dfy_proto::consts::{PERM_CONTROL, PERM_SYSTEM_ACTION, PERM_VIEW};
use dfy_proto::{InputEvent, Welcome};
use dfy_session::{
    serve_agent, AgentConfig, AgentRunStats, CaptureLoop, CtrlHandler, EncodeResolution,
    FrameInput, RuntimeSettings,
};
use dfy_transport::{bind_agent_with_identity, CtrlChannel, ServerAuth, TlsIdentity};
use quinn::{Connecting, Connection, Endpoint};
use tokio::time::{timeout, MissedTickBehavior};
use zeroize::Zeroizing;

use crate::controller::SYSTEM_ACTION_CAPABILITY;

pub const MAX_ENCODED_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_INPUT_EVENT_BYTES: usize = 1024;
const MAX_PENDING_HANDSHAKES: usize = 4;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub struct AgentSettings {
    pub password: Zeroizing<String>,
    pub allow_input: bool,
}

impl AgentSettings {
    pub fn new(password: String, allow_input: bool) -> Result<Self> {
        anyhow::ensure!(
            (6..=1024).contains(&password.len()) && !password.trim().is_empty(),
            "会话口令至少 6 位（6–1024 字节），且不能全为空白"
        );
        Ok(Self {
            password: Zeroizing::new(password),
            allow_input,
        })
    }
}

pub trait AgentCallbacks: Send + Sync {
    fn on_state(&self, state: &str, detail: &str);
    fn on_input(&self, event: &InputEvent);
    fn on_release_input(&self);
    /// 认证会话总数为零/非零的边界通知；待提交迁移也保持非零。
    fn on_capture_activity(&self, _active: bool) {}
    /// 应用平台编码器设置并返回实际生效值；默认实现必须明确拒绝。
    fn apply_runtime_settings(&self, settings: RuntimeSettings) -> Result<RuntimeSettings> {
        anyhow::bail!("Android 平台未实现运行时编码器设置：{settings:?}")
    }
    fn control_allowed(&self) -> bool {
        true
    }
}

#[derive(Debug, Default)]
pub struct AgentStats {
    pub sessions: AtomicU64,
    pub frames_sent: AtomicU64,
    pub packets_sent: AtomicU64,
    pub bytes_sent: AtomicU64,
    pub keyframes_sent: AtomicU64,
    pub input_events: AtomicU64,
    pub rejected_input_events: AtomicU64,
    pub auth_failures: AtomicU64,
}

impl AgentStats {
    pub fn add_session(&self, stats: &AgentRunStats) {
        self.sessions.fetch_add(1, Ordering::Relaxed);
        self.frames_sent
            .fetch_add(stats.frames_sent, Ordering::Relaxed);
        self.packets_sent
            .fetch_add(stats.packets_sent, Ordering::Relaxed);
        self.bytes_sent
            .fetch_add(stats.bytes_sent, Ordering::Relaxed);
        self.keyframes_sent
            .fetch_add(stats.keyframes_sent, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> String {
        format!(
            "{{\"sessions\":{},\"frames_sent\":{},\"packets_sent\":{},\"bytes_sent\":{},\"keyframes_sent\":{},\"input_events\":{},\"rejected_input_events\":{},\"auth_failures\":{}}}",
            self.sessions.load(Ordering::Relaxed),
            self.frames_sent.load(Ordering::Relaxed),
            self.packets_sent.load(Ordering::Relaxed),
            self.bytes_sent.load(Ordering::Relaxed),
            self.keyframes_sent.load(Ordering::Relaxed),
            self.input_events.load(Ordering::Relaxed),
            self.rejected_input_events.load(Ordering::Relaxed),
            self.auth_failures.load(Ordering::Relaxed),
        )
    }
}

struct BridgeHandler {
    allow_input: bool,
    callbacks: Arc<dyn AgentCallbacks>,
    stats: Arc<AgentStats>,
    input_was_allowed: bool,
}

impl BridgeHandler {
    fn new(allow_input: bool, callbacks: Arc<dyn AgentCallbacks>, stats: Arc<AgentStats>) -> Self {
        Self {
            allow_input,
            callbacks,
            stats,
            input_was_allowed: allow_input,
        }
    }

    fn accept(&self, event: &InputEvent) {
        if !self.allow_input || !self.callbacks.control_allowed() || !valid_input_event(event) {
            self.stats
                .rejected_input_events
                .fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.stats.input_events.fetch_add(1, Ordering::Relaxed);
        self.callbacks.on_input(event);
    }
}

impl CtrlHandler for BridgeHandler {
    fn on_ctrl(&mut self, env: &dfy_proto::Envelope) -> Result<Option<dfy_proto::Envelope>> {
        if let Some(dfy_proto::envelope::Body::InputEvent(event)) = &env.body {
            self.accept(event);
        }
        Ok(None)
    }

    fn on_datagram(&mut self, chan: u8, payload: &[u8]) -> Result<()> {
        if chan != dfy_proto::CHAN_INPUT_MOVE || payload.len() > MAX_INPUT_EVENT_BYTES {
            return Ok(());
        }
        let mut packet = Vec::with_capacity(payload.len() + 1);
        packet.push(chan);
        packet.extend_from_slice(payload);
        match dfy_proto::decode_input_move(&packet) {
            Ok(event) => self.accept(&event),
            Err(_) => {
                self.stats
                    .rejected_input_events
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(())
    }

    fn on_tick(&mut self) -> Result<()> {
        let allowed = self.allow_input && self.callbacks.control_allowed();
        if self.input_was_allowed && !allowed {
            anyhow::bail!("手势授权已撤销");
        }
        self.input_was_allowed = allowed;
        Ok(())
    }
}

fn valid_input_event(event: &InputEvent) -> bool {
    use dfy_proto::consts::{
        INPUT_KIND_KEY, INPUT_KIND_MOUSE_BTN, INPUT_KIND_MOUSE_MOVE, INPUT_KIND_TEXT,
        INPUT_KIND_WHEEL, INPUT_STATE_DOWN, INPUT_STATE_UP,
    };
    match event.kind {
        INPUT_KIND_KEY => {
            event.hid_usage != 0 && matches!(event.state, INPUT_STATE_DOWN | INPUT_STATE_UP)
        }
        INPUT_KIND_MOUSE_BTN => {
            (1..=5).contains(&event.hid_usage)
                && matches!(event.state, INPUT_STATE_DOWN | INPUT_STATE_UP)
        }
        INPUT_KIND_MOUSE_MOVE | INPUT_KIND_WHEEL => true,
        INPUT_KIND_TEXT => char::from_u32(event.unicode).is_some_and(|c| c != '\0'),
        _ => false,
    }
}

pub struct AgentHost {
    pub endpoint: Arc<Endpoint>,
    pub identity: TlsIdentity,
    pub frames: FrameInput,
    pub settings: AgentSettings,
    pub callbacks: Arc<dyn AgentCallbacks>,
    pub stats: Arc<AgentStats>,
    shutdown: Arc<Notify>,
}

impl AgentHost {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn bind(
        addr: SocketAddr,
        settings: AgentSettings,
        initial_runtime: RuntimeSettings,
        callbacks: Arc<dyn AgentCallbacks>,
        stats: Arc<AgentStats>,
    ) -> Result<(Self, CaptureLoop)> {
        let identity = TlsIdentity::generate("dfy-agent").context("生成 Android TLS 身份失败")?;
        Self::bind_with_identity(addr, identity, settings, initial_runtime, callbacks, stats)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn bind_with_identity(
        addr: SocketAddr,
        identity: TlsIdentity,
        settings: AgentSettings,
        initial_runtime: RuntimeSettings,
        callbacks: Arc<dyn AgentCallbacks>,
        stats: Arc<AgentStats>,
    ) -> Result<(Self, CaptureLoop)> {
        Self::bind_with_identity_and_source(
            addr,
            identity,
            settings,
            initial_runtime,
            initial_runtime.resolution,
            callbacks,
            stats,
        )
    }

    pub fn bind_with_identity_and_source(
        addr: SocketAddr,
        identity: TlsIdentity,
        settings: AgentSettings,
        initial_runtime: RuntimeSettings,
        source_resolution: Option<EncodeResolution>,
        callbacks: Arc<dyn AgentCallbacks>,
        stats: Arc<AgentStats>,
    ) -> Result<(Self, CaptureLoop)> {
        initial_runtime.validate()?;
        let (endpoint, identity) = bind_agent_with_identity(addr, identity)?;
        let apply_callbacks = Arc::clone(&callbacks);
        let activity_callbacks = Arc::clone(&callbacks);
        let (capture, frames) = CaptureLoop::external_with_runtime_and_source(
            16,
            initial_runtime,
            source_resolution,
            move |settings| apply_callbacks.apply_runtime_settings(settings),
        )?;
        capture.set_capture_activity_observer(move |active| {
            activity_callbacks.on_capture_activity(active);
        });
        let host = Self {
            endpoint: Arc::new(endpoint),
            identity,
            frames,
            settings,
            callbacks,
            stats,
            shutdown: Arc::new(Notify::new()),
        };
        Ok((host, capture))
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.endpoint.local_addr().context("读取监听地址失败")
    }

    pub fn fingerprint_hex(&self) -> String {
        self.identity.fingerprint_hex()
    }

    pub async fn run(self, mut capture: CaptureLoop) -> Result<()> {
        let settings = Arc::new(self.settings);
        let callbacks = Arc::clone(&self.callbacks);
        let stats = Arc::clone(&self.stats);
        let admission = Arc::new(Semaphore::new(MAX_PENDING_HANDSHAKES));
        let accepting = Arc::new(AtomicBool::new(true));
        let (pending_tx, mut pending_rx) =
            mpsc::channel::<(Connection, Welcome, CtrlChannel, bool)>(1);
        let mut ticker = tokio::time::interval(Duration::from_millis(250));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                incoming = self.endpoint.accept() => {
                    let Some(incoming) = incoming else { break };
                    if !accepting.load(Ordering::Acquire) {
                        incoming.refuse();
                        continue;
                    }
                    let Ok(permit) = admission.clone().try_acquire_owned() else {
                        incoming.refuse();
                        continue;
                    };
                    let connecting = match incoming.accept() {
                        Ok(connecting) => connecting,
                        Err(error) => {
                            tracing::debug!(%error, "Android agent: accept failed");
                            continue;
                        }
                    };
                    let settings = Arc::clone(&settings);
                    let callbacks = Arc::clone(&callbacks);
                    let stats = Arc::clone(&stats);
                    let pending_tx = pending_tx.clone();
                    tokio::spawn(async move {
                        let _permit = permit;
                        match authenticate(connecting, settings, callbacks, stats).await {
                            Ok(session) => {
                                if let Err(error) = pending_tx.send(session).await {
                                    error.0.0.close(0u32.into(), b"stopped");
                                }
                            }
                            Err(error) => tracing::debug!(%error, "Android agent: handshake rejected"),
                        }
                    });
                }
                session = pending_rx.recv() => {
                    let Some((conn, welcome, chan, allow_input)) = session else { break };
                    self.callbacks.on_state("connected", &welcome.agent_name);
                    capture.request_keyframe();
                    let handler = BridgeHandler::new(
                        allow_input,
                        Arc::clone(&self.callbacks),
                        Arc::clone(&self.stats),
                    );
                    let result = serve_agent(
                        conn,
                        welcome,
                        chan,
                        &mut capture,
                        handler,
                        AgentConfig {
                            stay_alive: true,
                            drain_delay: Duration::from_millis(100),
                            adaptation: Some(dfy_session::StreamAdaptationConfig::default()),
                            ..AgentConfig::default()
                        },
                    )
                    .await;
                    self.callbacks.on_release_input();
                    match result {
                        Ok(stats) => {
                            self.stats.add_session(&stats);
                            self.callbacks.on_state("disconnected", stats.end_reason);
                        }
                        Err(error) => self.callbacks.on_state("session_failed", &error.to_string()),
                    }
                }
                _ = self.shutdown.notified() => break,
                _ = ticker.tick() => {}
            }
        }
        accepting.store(false, Ordering::Release);
        self.endpoint.close(0u32.into(), b"stopped");
        capture.stop();
        self.callbacks.on_state("listening", "stopped");
        Ok(())
    }
}

async fn authenticate(
    connecting: Connecting,
    settings: Arc<AgentSettings>,
    callbacks: Arc<dyn AgentCallbacks>,
    stats: Arc<AgentStats>,
) -> Result<(Connection, Welcome, CtrlChannel, bool)> {
    let result = timeout(HANDSHAKE_TIMEOUT, async {
        let conn = connecting.await?;
        let control_allowed = settings.allow_input && callbacks.control_allowed();
        let mut auth = ServerAuth::new(settings.password.as_str(), "dfy-android");
        configure_auth(&mut auth, control_allowed);
        let (welcome, chan) = dfy_transport::server_handshake(&conn, &auth).await?;
        Ok::<_, anyhow::Error>((conn, welcome, chan, control_allowed))
    })
    .await
    .context("Android agent: handshake timed out")?;
    if let Err(error) = &result {
        stats.auth_failures.fetch_add(1, Ordering::Relaxed);
        callbacks.on_state("auth_failed", &error.to_string());
    }
    result
}

fn configure_auth(auth: &mut ServerAuth, control_allowed: bool) {
    auth.permission_bitmap = PERM_VIEW
        | if control_allowed {
            PERM_CONTROL | PERM_SYSTEM_ACTION
        } else {
            0
        };
    if control_allowed {
        if !auth
            .capabilities
            .iter()
            .any(|capability| capability == SYSTEM_ACTION_CAPABILITY)
        {
            auth.capabilities.push(SYSTEM_ACTION_CAPABILITY.to_string());
        }
    } else {
        auth.capabilities
            .retain(|capability| capability != SYSTEM_ACTION_CAPABILITY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_policy_matches_cli() {
        assert!(AgentSettings::new("12345".into(), false).is_err());
        assert!(AgentSettings::new("      ".into(), false).is_err());
        assert!(AgentSettings::new("123456".into(), false).is_ok());
        assert!(AgentSettings::new("0123456789abcdef".into(), false).is_ok());
        assert!(AgentSettings::new("x".repeat(1025), false).is_err());
    }

    #[test]
    fn input_validation_rejects_reserved_and_malformed_events() {
        let mut event = InputEvent {
            kind: dfy_proto::INPUT_KIND_TOUCH,
            ..InputEvent::default()
        };
        assert!(!valid_input_event(&event));
        event.kind = dfy_proto::INPUT_KIND_KEY;
        assert!(!valid_input_event(&event));
        event.hid_usage = 4;
        event.state = dfy_proto::INPUT_STATE_DOWN;
        assert!(valid_input_event(&event));
        event.state = 99;
        assert!(!valid_input_event(&event));
    }

    #[derive(Default)]
    struct TestCallbacks {
        allowed: AtomicBool,
        inputs: AtomicU64,
        applied: AtomicU64,
    }

    impl AgentCallbacks for TestCallbacks {
        fn on_state(&self, _state: &str, _detail: &str) {}
        fn on_input(&self, _event: &InputEvent) {
            self.inputs.fetch_add(1, Ordering::Relaxed);
        }
        fn on_release_input(&self) {}
        fn control_allowed(&self) -> bool {
            self.allowed.load(Ordering::Relaxed)
        }
        fn apply_runtime_settings(&self, settings: RuntimeSettings) -> Result<RuntimeSettings> {
            self.applied.fetch_add(1, Ordering::Relaxed);
            Ok(settings)
        }
    }

    #[test]
    fn runtime_callback_default_is_explicitly_unsupported() {
        struct Unsupported;
        impl AgentCallbacks for Unsupported {
            fn on_state(&self, _state: &str, _detail: &str) {}
            fn on_input(&self, _event: &InputEvent) {}
            fn on_release_input(&self) {}
        }
        let error = Unsupported
            .apply_runtime_settings(RuntimeSettings::new(30, 4000))
            .unwrap_err();
        assert!(error.to_string().contains("未实现"));
    }

    #[test]
    fn host_uses_real_initial_settings_and_platform_callback() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = runtime.enter();
        let callbacks = Arc::new(TestCallbacks::default());
        let initial = RuntimeSettings {
            fps: 30,
            bitrate_kbps: 4000,
            resolution: Some(dfy_session::EncodeResolution {
                width: 1920,
                height: 1080,
            }),
        };
        let (_host, capture) = AgentHost::bind(
            "127.0.0.1:0".parse().unwrap(),
            AgentSettings::new("0123456789abcdef".into(), false).unwrap(),
            initial,
            callbacks.clone(),
            Arc::new(AgentStats::default()),
        )
        .unwrap();
        assert_eq!(capture.runtime_control().current(), initial);
        let requested = RuntimeSettings::new(60, 5000);
        assert_eq!(
            capture.apply_runtime_settings(requested).unwrap(),
            requested
        );
        assert_eq!(callbacks.applied.load(Ordering::Relaxed), 1);
        assert_eq!(capture.runtime_control().current(), requested);
        capture.stop();
    }

    #[test]
    fn host_system_actions_require_live_input_authorization() {
        let mut auth = ServerAuth::new("0123456789abcdef", "test-android-host");
        configure_auth(&mut auth, false);
        assert_eq!(auth.permission_bitmap, PERM_VIEW);
        assert!(auth
            .capabilities
            .iter()
            .any(|cap| cap == "input-move.datagram.v1"));
        assert!(!auth
            .capabilities
            .iter()
            .any(|cap| cap == SYSTEM_ACTION_CAPABILITY));

        configure_auth(&mut auth, true);
        assert_eq!(
            auth.permission_bitmap,
            PERM_VIEW | PERM_CONTROL | PERM_SYSTEM_ACTION
        );
        assert!(auth
            .capabilities
            .iter()
            .any(|cap| cap == "input-move.datagram.v1"));
        assert_eq!(
            auth.capabilities
                .iter()
                .filter(|cap| cap.as_str() == SYSTEM_ACTION_CAPABILITY)
                .count(),
            1,
        );

        configure_auth(&mut auth, false);
        assert_eq!(auth.permission_bitmap, PERM_VIEW);
        assert!(auth
            .capabilities
            .iter()
            .any(|cap| cap == "input-move.datagram.v1"));
        assert!(!auth
            .capabilities
            .iter()
            .any(|cap| cap == SYSTEM_ACTION_CAPABILITY));
    }

    #[test]
    fn control_authorization_is_checked_live() {
        let callbacks = Arc::new(TestCallbacks::default());
        let stats = Arc::new(AgentStats::default());
        let mut handler = BridgeHandler::new(true, callbacks.clone(), stats.clone());
        let event = InputEvent {
            kind: dfy_proto::INPUT_KIND_KEY,
            hid_usage: 4,
            state: dfy_proto::INPUT_STATE_DOWN,
            ..InputEvent::default()
        };
        let envelope = dfy_proto::Envelope {
            body: Some(dfy_proto::envelope::Body::InputEvent(event.clone())),
        };
        handler.on_ctrl(&envelope).unwrap();
        assert_eq!(callbacks.inputs.load(Ordering::Relaxed), 0);
        assert_eq!(stats.rejected_input_events.load(Ordering::Relaxed), 1);

        callbacks.allowed.store(true, Ordering::Relaxed);
        handler.on_tick().unwrap();
        handler.on_ctrl(&envelope).unwrap();
        assert_eq!(callbacks.inputs.load(Ordering::Relaxed), 1);

        callbacks.allowed.store(false, Ordering::Relaxed);
        assert!(handler.on_tick().is_err());
        handler.on_ctrl(&envelope).unwrap();
        assert_eq!(callbacks.inputs.load(Ordering::Relaxed), 1);
        assert_eq!(stats.rejected_input_events.load(Ordering::Relaxed), 2);
    }
}
