// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (C) 2026 Dafeiyu Remote Project contributors

use std::{
    collections::HashMap,
    fmt,
    sync::atomic::{AtomicU64, Ordering},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{ensure, Context, Result};
use dfy_proto::{
    Welcome, CAPABILITY_QUALITY_CONTROL, PERM_CONTROL, SESSION_RESUME_COMMIT,
    SESSION_RESUME_PREPARE,
};
use dfy_transport::{
    send_video_frame, server_resume_request, CtrlSender, ResumeRegistry, ServerAuth, VideoSendStats,
};
use quinn::Connection;
use tokio::sync::watch;
use tracing::warn;

const DISCONNECTED_GRANT_TTL: Duration = Duration::from_secs(30);
static NEXT_LEASE_ID: AtomicU64 = AtomicU64::new(1);

/// COMMIT 已经改变会话所有权，但 `SessionResumeOk` 未能发出。
///
/// 调用方必须保留已提交状态并允许幂等重试，不能把它当普通拒绝回滚。
#[derive(Debug)]
pub struct MigrationAckFailed(pub anyhow::Error);

impl fmt::Display for MigrationAckFailed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("migration COMMIT succeeded but its acknowledgement failed")
    }
}

impl std::error::Error for MigrationAckFailed {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

fn next_lease_id() -> u64 {
    NEXT_LEASE_ID.fetch_add(1, Ordering::Relaxed)
}

#[derive(Clone, Default)]
pub struct SessionCoordinator {
    inner: Arc<Mutex<HashMap<Vec<u8>, Arc<SharedSession>>>>,
}

struct SharedState {
    generation: u64,
    owner: u64,
    frame_id: u32,
    frame_seq: u16,
    epoch: u64,
    token: Vec<u8>,
    connections: usize,
    disconnected_at: Option<Instant>,
    changed: watch::Sender<u64>,
}

type SharedSession = Mutex<SharedState>;

impl fmt::Debug for SessionCoordinator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionCoordinator").finish_non_exhaustive()
    }
}

impl SessionCoordinator {
    fn prune(&self, registry: &ResumeRegistry) {
        let Ok(mut sessions) = self.inner.lock() else {
            return;
        };
        let now = Instant::now();
        sessions.retain(|session_id, shared| {
            let Ok(shared) = shared.lock() else {
                return false;
            };
            let expired = shared.connections == 0
                && shared
                    .disconnected_at
                    .is_some_and(|since| now.duration_since(since) >= DISCONNECTED_GRANT_TTL);
            if expired {
                registry.revoke(session_id);
            }
            !expired
        });
    }

    fn register(&self, welcome: &Welcome, lease_id: u64) -> Result<Arc<SharedSession>> {
        ensure!(
            welcome.session_id.len() == dfy_proto::SESSION_ID_LEN,
            "initial session id has an invalid length"
        );
        let mut sessions = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("session coordinator lock poisoned"))?;
        if let Some(shared) = sessions.get(&welcome.session_id) {
            let shared = shared.clone();
            let mut state = shared
                .lock()
                .map_err(|_| anyhow::anyhow!("session state lock poisoned"))?;
            ensure!(
                state.epoch == welcome.session_epoch && state.token == welcome.resume_token,
                "session lease does not match the active generation"
            );
            state.generation = state.generation.wrapping_add(1);
            state.owner = lease_id;
            state.connections += 1;
            state.disconnected_at = None;
            state.changed.send_modify(|generation| {
                *generation = generation.wrapping_add(1);
            });
            drop(state);
            return Ok(shared);
        }
        let shared = Arc::new(Mutex::new(SharedState {
            generation: 1,
            owner: lease_id,
            frame_id: 0,
            frame_seq: 0,
            epoch: welcome.session_epoch,
            token: welcome.resume_token.clone(),
            connections: 1,
            disconnected_at: None,
            changed: watch::channel(1).0,
        }));
        sessions.insert(welcome.session_id.clone(), shared.clone());
        Ok(shared)
    }

    pub fn contains(&self, session_id: &[u8]) -> bool {
        self.inner
            .lock()
            .is_ok_and(|sessions| sessions.contains_key(session_id))
    }

    fn find(&self, session_id: &[u8]) -> Result<Arc<SharedSession>> {
        self.inner
            .lock()
            .map_err(|_| anyhow::anyhow!("session coordinator lock poisoned"))?
            .get(session_id)
            .cloned()
            .with_context(|| format!("unknown logical session {}", hex::encode(session_id)))
    }

    fn remove(&self, session_id: &[u8]) {
        if let Ok(mut sessions) = self.inner.lock() {
            sessions.remove(session_id);
        }
    }
}

#[derive(Clone)]
pub struct MigrationContext {
    pub auth: ServerAuth,
    pub hello: dfy_proto::Hello,
    pub exporter: [u8; 32],
    pub coordinator: SessionCoordinator,
}

impl fmt::Debug for MigrationContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MigrationContext")
            .field("hello", &self.hello)
            .field("coordinator", &self.coordinator)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct SessionLease {
    id: u64,
    coordinator: SessionCoordinator,
    shared: Option<Arc<SharedSession>>,
    generation: u64,
    active: bool,
    pending_session_id: Vec<u8>,
    permission_bitmap: u32,
    capability: Vec<String>,
    registry: ResumeRegistry,
    changed: Option<watch::Sender<u64>>,
}

impl SessionLease {
    pub(crate) fn for_welcome(context: &MigrationContext, welcome: &Welcome) -> Result<Self> {
        if context.hello.resume_session_id.is_empty() {
            Self::initial(context, welcome)
        } else {
            Ok(Self::pending(context, welcome))
        }
    }

    pub fn initial(context: &MigrationContext, welcome: &Welcome) -> Result<Self> {
        context.coordinator.prune(&context.auth.resume_registry);
        let id = next_lease_id();
        let shared = context.coordinator.register(welcome, id)?;
        let generation = shared
            .lock()
            .map_err(|_| anyhow::anyhow!("session state lock poisoned"))?
            .generation;
        let changed = shared
            .lock()
            .map_err(|_| anyhow::anyhow!("session state lock poisoned"))?
            .changed
            .clone();
        Ok(Self {
            id,
            coordinator: context.coordinator.clone(),
            generation,
            shared: Some(shared),
            active: true,
            pending_session_id: welcome.session_id.clone(),
            permission_bitmap: welcome.permission_bitmap,
            capability: welcome.capability.clone(),
            registry: context.auth.resume_registry.clone(),
            changed: Some(changed),
        })
    }

    pub fn pending(context: &MigrationContext, welcome: &Welcome) -> Self {
        Self {
            id: next_lease_id(),
            coordinator: context.coordinator.clone(),
            shared: None,
            generation: 0,
            active: false,
            pending_session_id: welcome.session_id.clone(),
            permission_bitmap: 0,
            capability: Vec::new(),
            registry: context.auth.resume_registry.clone(),
            changed: None,
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn is_pending(&self) -> bool {
        self.shared.is_none()
    }

    pub fn permissions(&self) -> u32 {
        self.permission_bitmap
    }

    pub fn capabilities(&self) -> &[String] {
        &self.capability
    }

    pub fn quality_control_allowed(&self) -> bool {
        self.active
            && !self.is_pending()
            && self.permission_bitmap & PERM_CONTROL != 0
            && self
                .capability
                .iter()
                .any(|capability| capability == CAPABILITY_QUALITY_CONTROL)
    }

    pub fn was_superseded(&self) -> bool {
        self.shared.as_ref().is_some_and(|shared| {
            shared
                .lock()
                .is_ok_and(|state| self.generation != state.generation)
        })
    }

    pub async fn changed(&self) {
        let Some(changed) = &self.changed else {
            return std::future::pending::<()>().await;
        };
        let mut receiver = changed.subscribe();
        let _ = receiver.borrow_and_update();
        if receiver.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }

    pub fn next_frame_id(&self) -> Option<u64> {
        let shared = self.shared.as_ref()?;
        if !self.active {
            return None;
        }
        let state = shared.lock().ok()?;
        (self.generation == state.generation).then_some(u64::from(state.frame_id))
    }

    pub fn send_frame(
        &self,
        conn: &Connection,
        pts_us: u32,
        frame: &[u8],
        keyframe: bool,
    ) -> Result<Option<VideoSendStats>> {
        let Some(shared) = &self.shared else {
            return Ok(None);
        };
        let mut state = shared
            .lock()
            .map_err(|_| anyhow::anyhow!("session state lock poisoned"))?;
        if !self.active || self.generation != state.generation {
            return Ok(None);
        }
        let sent = send_video_frame(
            conn,
            state.frame_id,
            state.frame_seq,
            pts_us,
            frame,
            keyframe,
        )?;
        state.frame_id = state.frame_id.wrapping_add(1);
        state.frame_seq = state.frame_seq.wrapping_add(1);
        Ok(Some(sent))
    }

    pub async fn handle_resume(
        &mut self,
        context: &MigrationContext,
        chan: &mut CtrlSender,
        request: &dfy_proto::SessionResume,
    ) -> Result<()> {
        self.handle_resume_with_commit(context, chan, request, || Ok(()))
            .await
    }

    /// Handle a resume request, optionally committing capture ownership before acknowledging
    /// a COMMIT. The hook runs only for COMMIT and failure prevents `SessionResumeOk`.
    pub async fn handle_resume_with_commit<F>(
        &mut self,
        context: &MigrationContext,
        chan: &mut CtrlSender,
        request: &dfy_proto::SessionResume,
        commit: F,
    ) -> Result<()>
    where
        F: FnOnce() -> Result<()>,
    {
        ensure!(
            matches!(
                request.phase,
                SESSION_RESUME_PREPARE | SESSION_RESUME_COMMIT
            ),
            "legacy or unknown session resume phase {}",
            request.phase
        );
        ensure!(
            context.hello.resume_session_id == request.session_id,
            "resume target was not declared during authentication"
        );
        if request.phase == SESSION_RESUME_COMMIT {
            commit()?;
        }
        let shared = context.coordinator.find(&request.session_id)?;
        let resume_from = {
            let state = shared
                .lock()
                .map_err(|_| anyhow::anyhow!("session state lock poisoned"))?;
            u64::from(state.frame_id)
        };
        let response = match server_resume_request(
            &context.auth,
            &context.hello,
            &context.exporter,
            request,
            resume_from,
        ) {
            Ok(response) => response,
            Err(error) => {
                warn!(error = %error, "rejected session migration");
                return Err(error);
            }
        };
        if request.phase == SESSION_RESUME_COMMIT {
            let mut state = shared
                .lock()
                .map_err(|_| anyhow::anyhow!("session state lock poisoned"))?;
            if state.epoch == response.session_epoch && state.token == response.resume_token {
                if state.connections == 0 {
                    // 已提交但确认丢失的连接可能已经全部断开。 grant 仍处于 TTL 内时，
                    // 新 lease 可 reclaim 同一已提交代次，保持 epoch/token/帧进度不变。
                    state.owner = self.id;
                } else {
                    ensure!(
                        state.owner == self.id,
                        "COMMIT was already claimed by another connection"
                    );
                }
            } else {
                let generation = state
                    .generation
                    .checked_add(1)
                    .context("session generation overflow")?;
                ensure!(
                    state.epoch.checked_add(1) == Some(response.session_epoch),
                    "migration epoch does not follow the active session"
                );
                state.epoch = response.session_epoch;
                state.token = response.resume_token.clone();
                state.generation = generation;
                state.owner = self.id;
                state.changed.send_modify(|generation| {
                    *generation = generation.wrapping_add(1);
                });
            }
            let already_leased = self.active
                && self
                    .shared
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &shared))
                && self.generation == state.generation;
            if !already_leased {
                state.connections += 1;
            }
            state.disconnected_at = None;
            self.generation = state.generation;
            self.shared = Some(shared.clone());
            self.changed = Some(state.changed.clone());
            self.active = true;
            self.registry.revoke(&self.pending_session_id);
            self.pending_session_id = request.session_id.clone();
            self.permission_bitmap = response.permission_bitmap;
            self.capability = response.capability.clone();
        }
        if let Err(error) = chan
            .send_ctrl(&dfy_proto::Envelope {
                body: Some(dfy_proto::envelope::Body::SessionResumeOk(response)),
            })
            .await
        {
            if request.phase == SESSION_RESUME_COMMIT {
                return Err(MigrationAckFailed(error).into());
            }
            return Err(error);
        }
        Ok(())
    }

    pub fn finish(&mut self, context: &MigrationContext, user_ended: bool) {
        if let Some(shared) = &self.shared {
            if let Ok(mut state) = shared.lock() {
                state.connections = state.connections.saturating_sub(1);
                if state.connections == 0 {
                    state.disconnected_at = Some(Instant::now());
                }
            }
        } else {
            self.registry.revoke(&self.pending_session_id);
        }
        if user_ended {
            self.registry.revoke(&self.pending_session_id);
            self.coordinator.remove(&self.pending_session_id);
        }
        self.active = false;
        self.shared = None;
        context.coordinator.prune(&context.auth.resume_registry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disconnected_grants_have_a_bounded_migration_window() {
        assert_eq!(DISCONNECTED_GRANT_TTL, Duration::from_secs(30));
    }

    #[test]
    fn only_declared_migration_connection_waits_for_commit() {
        let mut context = MigrationContext {
            auth: ServerAuth::new("0123456789abcdef", "lease-test"),
            hello: dfy_proto::Hello {
                proto_major: 1,
                proto_minor: 0,
                client_name: "lease-test".into(),
                device_pubkey: Vec::new(),
                client_nonce: Vec::new(),
                resume_session_id: Vec::new(),
            },
            exporter: [0; 32],
            coordinator: SessionCoordinator::default(),
        };
        let welcome = Welcome {
            session_id: vec![7; dfy_proto::SESSION_ID_LEN],
            agent_name: "lease-test".into(),
            capability: vec![CAPABILITY_QUALITY_CONTROL.into()],
            permission_bitmap: dfy_proto::PERM_VIEW | PERM_CONTROL,
            resume_token: vec![9; 32],
            session_epoch: 0,
        };
        let initial = SessionLease::for_welcome(&context, &welcome).unwrap();
        assert!(initial.is_active());
        assert!(!initial.is_pending());
        assert!(initial.quality_control_allowed());

        context.hello.resume_session_id = vec![6; dfy_proto::SESSION_ID_LEN];
        let pending = SessionLease::for_welcome(&context, &welcome).unwrap();
        assert!(pending.is_pending());
        assert!(!pending.is_active());
        assert!(!pending.quality_control_allowed());
        assert_eq!(pending.permissions(), 0);
        assert!(pending.capabilities().is_empty());
    }
}
