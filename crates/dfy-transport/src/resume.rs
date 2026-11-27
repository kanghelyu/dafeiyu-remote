//! 会话迁移：设备私钥签名、TLS 通道绑定、一次性令牌与原子提交。

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{bail, ensure, Context, Result};
use dfy_proto::{
    Hello, SessionResume, RESUME_NONCE_LEN, RESUME_TOKEN_LEN, SESSION_ID_LEN,
    SESSION_RESUME_COMMIT, SESSION_RESUME_PREPARE,
};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rand_core::RngCore;
use zeroize::Zeroizing;

const TRANSCRIPT_DOMAIN: &[u8] = b"DFYP-SESSION-RESUME-V1";
const PENDING_TTL: Duration = Duration::from_secs(30);
const COMMIT_TTL: Duration = Duration::from_secs(30);
const MAX_ACTIVE_GRANTS: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResumeGrant {
    pub session_id: Vec<u8>,
    pub session_epoch: u64,
    pub device_pubkey: [u8; 32],
    pub permission_bitmap: u32,
    pub capability: Vec<String>,
    pub resume_token: Vec<u8>,
}

#[derive(Clone)]
pub struct ResumePreparation {
    pub grant: ResumeGrant,
    pub server_nonce: [u8; RESUME_NONCE_LEN],
    pub client_nonce: [u8; RESUME_NONCE_LEN],
    pub last_frame_id: u64,
    pub last_frame_seq: u64,
}

#[derive(Clone)]
struct Pending {
    preparation: ResumePreparation,
    expires_at: Instant,
}

struct Committed {
    response: CommittedResponse,
    request: SessionResume,
    expires_at: Instant,
}

#[derive(Clone)]
pub struct CommittedResponse {
    pub grant: ResumeGrant,
    pub server_nonce: [u8; RESUME_NONCE_LEN],
}

#[derive(Default)]
struct State {
    active: HashMap<Vec<u8>, ResumeGrant>,
    pending: HashMap<Vec<u8>, Pending>,
    committed: HashMap<Vec<u8>, Committed>,
}

#[derive(Clone, Default)]
pub struct ResumeRegistry {
    state: Arc<Mutex<State>>,
}

impl ResumeRegistry {
    pub fn new_grant(&self, grant: ResumeGrant) -> Result<ResumeGrant> {
        ensure!(
            grant.session_id.len() == SESSION_ID_LEN,
            "逻辑会话 ID 必须为 16B"
        );
        let mut token = Zeroizing::new(vec![0u8; RESUME_TOKEN_LEN]);
        rand::rngs::OsRng.fill_bytes(token.as_mut_slice());
        let grant = ResumeGrant {
            resume_token: token.to_vec(),
            ..grant
        };
        self.issue(grant.clone())?;
        Ok(grant)
    }

    pub fn issue(&self, grant: ResumeGrant) -> Result<()> {
        validate_grant(&grant)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("恢复注册表锁已中毒"))?;
        ensure!(
            state.active.len() < MAX_ACTIVE_GRANTS,
            "活跃恢复授权过多，请先结束旧会话"
        );
        let old = state
            .active
            .insert(grant.resume_token.clone(), grant.clone());
        if let Some(old) = old {
            state.pending.remove(&old.resume_token);
        }
        Ok(())
    }

    pub fn revoke(&self, session_id: &[u8]) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let tokens: Vec<_> = state
            .active
            .values()
            .filter(|grant| grant.session_id == session_id)
            .map(|grant| grant.resume_token.clone())
            .collect();
        for token in tokens {
            state.active.remove(&token);
            state.pending.remove(&token);
            state.committed.remove(&token);
        }
    }

    pub fn prepare(
        &self,
        hello: &Hello,
        exporter: &[u8; 32],
        request: &SessionResume,
    ) -> Result<ResumePreparation> {
        validate_request(request, SESSION_RESUME_PREPARE)?;
        let device_pubkey = verifying_key(hello)?;
        verify_signature(&device_pubkey, hello, exporter, request)?;
        let now = Instant::now();
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("恢复注册表锁已中毒"))?;
        state.pending.retain(|_, pending| pending.expires_at > now);
        state
            .committed
            .retain(|_, committed| committed.expires_at > now);
        let token = request.resume_token.as_slice();
        if state.committed.contains_key(token) {
            bail!("恢复令牌已经提交");
        }
        let grant = state
            .active
            .get(token)
            .cloned()
            .context("恢复令牌未知、已过期或已被使用")?;
        ensure!(
            grant.session_id == request.session_id,
            "恢复令牌不属于该会话"
        );
        ensure!(
            grant.device_pubkey == device_pubkey.to_bytes(),
            "恢复设备身份不匹配"
        );
        ensure!(
            request.permission_bitmap & !grant.permission_bitmap == 0,
            "迁移不能扩大权限"
        );
        if let Some(pending) = state.pending.get(token) {
            ensure!(
                pending.preparation.client_nonce.as_slice() == request.client_nonce,
                "同一令牌已有待提交恢复"
            );
            return Ok(pending.preparation.clone());
        }
        let mut server_nonce = [0u8; RESUME_NONCE_LEN];
        rand::rngs::OsRng.fill_bytes(&mut server_nonce);
        let preparation = ResumePreparation {
            grant: ResumeGrant {
                permission_bitmap: request.permission_bitmap,
                resume_token: grant.resume_token.clone(),
                ..grant
            },
            server_nonce,
            client_nonce: request.client_nonce.clone().try_into().expect("长度已验证"),
            last_frame_id: request.last_frame_id,
            last_frame_seq: request.last_frame_seq,
        };
        state.pending.insert(
            token.to_vec(),
            Pending {
                preparation: preparation.clone(),
                expires_at: now + PENDING_TTL,
            },
        );
        Ok(preparation)
    }

    pub fn commit(
        &self,
        hello: &Hello,
        exporter: &[u8; 32],
        request: &SessionResume,
    ) -> Result<CommittedResponse> {
        validate_request(request, SESSION_RESUME_COMMIT)?;
        let device_pubkey = verifying_key(hello)?;
        verify_signature(&device_pubkey, hello, exporter, request)?;
        let now = Instant::now();
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("恢复注册表锁已中毒"))?;
        state
            .committed
            .retain(|_, committed| committed.expires_at > now);
        let token = request.resume_token.as_slice();
        if let Some(committed) = state.committed.get(token) {
            ensure!(
                committed.response.grant.device_pubkey == device_pubkey.to_bytes(),
                "重复 COMMIT 的设备身份不匹配"
            );
            let mut expected = committed.request.clone();
            expected.signature.clear();
            let mut actual = request.clone();
            actual.signature.clear();
            ensure!(expected == actual, "重复 COMMIT 的请求字段不匹配");
            return Ok(committed.response.clone());
        }
        let pending = state
            .pending
            .get(token)
            .filter(|pending| pending.expires_at > now)
            .cloned()
            .context("没有有效的 PREPARE，不能提交迁移")?;
        let preparation = pending.preparation;
        ensure!(
            preparation.grant.session_id == request.session_id,
            "恢复会话不匹配"
        );
        ensure!(
            preparation.grant.device_pubkey == device_pubkey.to_bytes(),
            "恢复设备不匹配"
        );
        ensure!(
            preparation.server_nonce.as_slice() == request.server_nonce,
            "服务端随机数不匹配"
        );
        ensure!(
            preparation.client_nonce.as_slice() == request.client_nonce,
            "客户端随机数不匹配"
        );
        ensure!(
            request.permission_bitmap == preparation.grant.permission_bitmap,
            "COMMIT 权限必须与 PREPARE 一致"
        );
        ensure!(
            request.last_frame_id == preparation.last_frame_id
                && request.last_frame_seq == preparation.last_frame_seq,
            "COMMIT 帧进度必须与 PREPARE 一致"
        );
        let mut new_token = Zeroizing::new(vec![0u8; RESUME_TOKEN_LEN]);
        rand::rngs::OsRng.fill_bytes(new_token.as_mut_slice());
        let grant = ResumeGrant {
            session_epoch: preparation
                .grant
                .session_epoch
                .checked_add(1)
                .context("会话代数溢出")?,
            resume_token: new_token.to_vec(),
            ..preparation.grant
        };
        state.pending.remove(token);
        state.active.remove(token);
        state
            .active
            .insert(grant.resume_token.clone(), grant.clone());
        let response = CommittedResponse {
            grant,
            server_nonce: preparation.server_nonce,
        };
        state.committed.insert(
            token.to_vec(),
            Committed {
                response: response.clone(),
                request: request.clone(),
                expires_at: now + COMMIT_TTL,
            },
        );
        Ok(response)
    }
}

pub fn sign_request(
    signing_key: &SigningKey,
    hello: &Hello,
    exporter: &[u8; 32],
    request: &SessionResume,
) -> Result<Vec<u8>> {
    ensure!(
        hello.device_pubkey == signing_key.verifying_key().to_bytes(),
        "Hello 设备公钥与签名私钥不匹配"
    );
    let phase = match request.phase {
        SESSION_RESUME_PREPARE if request.server_nonce.is_empty() => SESSION_RESUME_PREPARE,
        SESSION_RESUME_COMMIT => SESSION_RESUME_COMMIT,
        _ => bail!("无效的迁移阶段或 PREPARE 服务端随机数"),
    };
    validate_request(request, phase)?;
    Ok(signing_key
        .sign(&resume_transcript(hello, exporter, request))
        .to_bytes()
        .to_vec())
}

pub fn resume_transcript(hello: &Hello, exporter: &[u8; 32], request: &SessionResume) -> Vec<u8> {
    let mut out = Vec::new();
    for field in [
        TRANSCRIPT_DOMAIN,
        &request.phase.to_be_bytes(),
        &request.last_frame_id.to_be_bytes(),
        &request.last_frame_seq.to_be_bytes(),
        &request.session_id,
        &request.resume_token,
        &request.client_nonce,
        &request.server_nonce,
        &request.permission_bitmap.to_be_bytes(),
        &hello.device_pubkey,
        &hello.client_nonce,
        hello.client_name.as_bytes(),
        exporter,
    ] {
        append_field(&mut out, field);
    }
    out
}

fn append_field(out: &mut Vec<u8>, field: &[u8]) {
    let len = u32::try_from(field.len()).expect("迁移字段超过 u32 长度");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(field);
}

fn verifying_key(hello: &Hello) -> Result<VerifyingKey> {
    VerifyingKey::from_bytes(
        &hello
            .device_pubkey
            .clone()
            .try_into()
            .map_err(|_| anyhow::anyhow!("Hello.device_pubkey 必须为 32B Ed25519 公钥"))?,
    )
    .context("Hello.device_pubkey 不是有效 Ed25519 公钥")
}

fn verify_signature(
    key: &VerifyingKey,
    hello: &Hello,
    exporter: &[u8; 32],
    request: &SessionResume,
) -> Result<()> {
    let signature = Signature::from_bytes(
        &request
            .signature
            .clone()
            .try_into()
            .map_err(|_| anyhow::anyhow!("迁移签名长度必须为 64B"))?,
    );
    key.verify_strict(&resume_transcript(hello, exporter, request), &signature)
        .context("迁移签名或 TLS 通道绑定校验失败")
}

fn validate_request(request: &SessionResume, phase: u32) -> Result<()> {
    ensure!(request.phase == phase, "迁移阶段不匹配");
    ensure!(
        request.session_id.len() == SESSION_ID_LEN,
        "逻辑会话 ID 必须为 16B"
    );
    ensure!(
        request.resume_token.len() == RESUME_TOKEN_LEN,
        "恢复令牌必须为 32B"
    );
    ensure!(
        request.client_nonce.len() == RESUME_NONCE_LEN,
        "客户端随机数必须为 32B"
    );
    if phase == SESSION_RESUME_PREPARE {
        ensure!(
            request.server_nonce.is_empty(),
            "PREPARE 不能携带服务端随机数"
        );
    } else {
        ensure!(
            request.server_nonce.len() == RESUME_NONCE_LEN,
            "COMMIT 服务端随机数必须为 32B"
        );
    }
    Ok(())
}

fn validate_grant(grant: &ResumeGrant) -> Result<()> {
    ensure!(
        grant.session_id.len() == SESSION_ID_LEN,
        "逻辑会话 ID 必须为 16B"
    );
    ensure!(
        grant.resume_token.len() == RESUME_TOKEN_LEN,
        "恢复令牌必须为 32B"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn fixture() -> (SigningKey, Hello, [u8; 32], ResumeRegistry, ResumeGrant) {
        let signing_key = SigningKey::from_bytes(&[7; 32]);
        let hello = Hello {
            proto_major: 1,
            proto_minor: 2,
            client_name: "test".into(),
            device_pubkey: signing_key.verifying_key().to_bytes().to_vec(),
            client_nonce: vec![4; 32],
            resume_session_id: vec![1; 16],
        };
        let exporter = [5; 32];
        let registry = ResumeRegistry::default();
        let grant = ResumeGrant {
            session_id: vec![1; 16],
            session_epoch: 3,
            device_pubkey: signing_key.verifying_key().to_bytes(),
            permission_bitmap: 0b111,
            capability: vec!["ctrl.frame.v1".into()],
            resume_token: vec![9; 32],
        };
        registry.issue(grant.clone()).unwrap();
        (signing_key, hello, exporter, registry, grant)
    }

    fn request(phase: u32) -> SessionResume {
        SessionResume {
            last_frame_id: 42,
            last_frame_seq: 2,
            session_id: vec![1; 16],
            resume_token: vec![9; 32],
            phase,
            client_nonce: vec![8; 32],
            server_nonce: Vec::new(),
            signature: Vec::new(),
            permission_bitmap: 0b11,
        }
    }

    #[test]
    fn commit_rotates_token_freezes_permissions_and_is_idempotent() {
        let (key, hello, exporter, registry, _) = fixture();
        let mut prepare = request(SESSION_RESUME_PREPARE);
        prepare.signature = sign_request(&key, &hello, &exporter, &prepare).unwrap();
        let prepared = registry.prepare(&hello, &exporter, &prepare).unwrap();
        assert_eq!(prepared.grant.permission_bitmap, 0b11);

        let mut commit = prepare.clone();
        commit.phase = SESSION_RESUME_COMMIT;
        commit.server_nonce = prepared.server_nonce.to_vec();
        commit.signature = sign_request(&key, &hello, &exporter, &commit).unwrap();
        let committed = registry.commit(&hello, &exporter, &commit).unwrap();
        assert_eq!(committed.grant.session_epoch, 4);
        assert_eq!(committed.grant.resume_token.len(), 32);
        assert_ne!(committed.grant.resume_token, prepare.resume_token);
        assert_eq!(
            registry.commit(&hello, &exporter, &commit).unwrap().grant,
            committed.grant
        );
        let mut replay_hello = hello.clone();
        replay_hello.client_nonce = vec![10; 32];
        let replay_exporter = [6; 32];
        let mut replay = commit.clone();
        replay.signature = sign_request(&key, &replay_hello, &replay_exporter, &replay).unwrap();
        assert_eq!(
            registry
                .commit(&replay_hello, &replay_exporter, &replay)
                .unwrap()
                .grant,
            committed.grant
        );
        let mut changed = replay;
        changed.last_frame_id += 1;
        changed.signature = sign_request(&key, &replay_hello, &replay_exporter, &changed).unwrap();
        assert!(registry
            .commit(&replay_hello, &replay_exporter, &changed)
            .is_err());
        let other_key = SigningKey::from_bytes(&[9; 32]);
        let mut other_hello = replay_hello.clone();
        other_hello.device_pubkey = other_key.verifying_key().to_bytes().to_vec();
        let mut other_replay = commit.clone();
        other_replay.signature =
            sign_request(&other_key, &other_hello, &replay_exporter, &other_replay).unwrap();
        assert!(registry
            .commit(&other_hello, &replay_exporter, &other_replay)
            .is_err());
        registry
            .state
            .lock()
            .unwrap()
            .committed
            .get_mut(commit.resume_token.as_slice())
            .unwrap()
            .expires_at = Instant::now() - Duration::from_secs(1);
        assert!(registry.commit(&hello, &exporter, &commit).is_err());
        assert!(registry.prepare(&hello, &exporter, &prepare).is_err());
    }

    #[test]
    fn signature_binds_channel_device_phase_progress_and_permissions() {
        let (key, hello, exporter, _, _) = fixture();
        let mut original = request(SESSION_RESUME_PREPARE);
        original.signature = sign_request(&key, &hello, &exporter, &original).unwrap();
        let registry = ResumeRegistry::default();
        for index in 0..8 {
            let mut changed = original.clone();
            match index {
                0 => changed.phase = SESSION_RESUME_COMMIT,
                1 => changed.last_frame_id += 1,
                2 => changed.last_frame_seq += 1,
                3 => changed.session_id[0] ^= 1,
                4 => changed.resume_token[0] ^= 1,
                5 => changed.client_nonce[0] ^= 1,
                6 => changed.permission_bitmap = 1,
                _ => changed.server_nonce = vec![0; 32],
            }
            assert!(
                registry.prepare(&hello, &exporter, &changed).is_err(),
                "variant {index}"
            );
        }
        assert!(registry.prepare(&hello, &[6; 32], &original).is_err());
        let mut other_hello = hello.clone();
        other_hello.device_pubkey = SigningKey::from_bytes(&[8; 32])
            .verifying_key()
            .to_bytes()
            .to_vec();
        assert!(registry
            .prepare(&other_hello, &exporter, &original)
            .is_err());
    }

    #[test]
    fn migration_cannot_expand_permissions_or_skip_prepare() {
        let (key, hello, exporter, registry, _) = fixture();
        let mut prepare = request(SESSION_RESUME_PREPARE);
        prepare.permission_bitmap = 0b1000;
        prepare.signature = sign_request(&key, &hello, &exporter, &prepare).unwrap();
        assert!(registry.prepare(&hello, &exporter, &prepare).is_err());

        let mut commit = request(SESSION_RESUME_COMMIT);
        commit.server_nonce = vec![0; 32];
        commit.signature = sign_request(&key, &hello, &exporter, &commit).unwrap();
        assert!(registry.commit(&hello, &exporter, &commit).is_err());
    }
}
