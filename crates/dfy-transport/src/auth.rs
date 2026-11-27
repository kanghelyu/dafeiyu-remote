//! DFYP 认证 v2：固定 Argon2id 成本、TLS exporter 绑定和角色分离的口令互证。
//!
//! 该方案不是 PAKE，也不证明 Hello 中设备公钥的私钥所有权。它阻止不知口令的
//! TLS 终结中继转发证明；持久设备身份、配对、撤销和安全密钥存储由上层实现。

use std::sync::{Arc, OnceLock};

use anyhow::{anyhow, ensure, Context, Result};
use argon2::{Algorithm, Argon2, Params, Version};
use dfy_proto::Hello;
use hmac::{Hmac, Mac};
use quinn::Connection;
use sha2::Sha256;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use zeroize::Zeroizing;

pub(crate) const AUTH_VERSION: u32 = 2;
pub(crate) const AUTH_MIN_PROTO_MINOR: u32 = 2;
pub(crate) const NONCE_LEN: usize = 32;
pub(crate) const SALT_LEN: usize = 16;
pub(crate) const PROOF_LEN: usize = 32;
const EXPORTER_LABEL: &[u8] = b"EXPORTER-DFYP-1-AUTH-V2";
const ARGON_MEMORY_KIB: u32 = 19_456;
const ARGON_ITERATIONS: u32 = 2;
const ARGON_PARALLELISM: u32 = 1;
const KEY_LEN: usize = 32;
// 全进程至多 2 个 KDF（约 38 MiB Argon2 工作内存）和 8 个异步等待者。
const KDF_CONCURRENCY: usize = 2;
const KDF_QUEUE_CAPACITY: usize = 8;
static KDF_LIMITER: OnceLock<KdfLimiter> = OnceLock::new();

type HmacSha256 = Hmac<Sha256>;
pub(crate) type PasswordKey = Zeroizing<[u8; KEY_LEN]>;

struct KdfLimiter {
    admission: Arc<Semaphore>,
    active: Arc<Semaphore>,
}

struct KdfPermit {
    _admission: OwnedSemaphorePermit,
    _active: OwnedSemaphorePermit,
}

impl KdfLimiter {
    fn new(concurrency: usize, queue_capacity: usize) -> Self {
        Self {
            admission: Arc::new(Semaphore::new(concurrency + queue_capacity)),
            active: Arc::new(Semaphore::new(concurrency)),
        }
    }

    async fn acquire(&self) -> Result<KdfPermit> {
        // try_acquire 在等待 active 之前执行，不能为满队列继续创建等待者。
        let admission = self
            .admission
            .clone()
            .try_acquire_owned()
            .map_err(|_| anyhow!("认证 KDF 队列已满，请稍后重试"))?;
        let active = self
            .active
            .clone()
            .acquire_owned()
            .await
            .context("认证 KDF 并发限制器已关闭")?;
        Ok(KdfPermit {
            _admission: admission,
            _active: active,
        })
    }
}

/// 固定参数不由网络协商。permit 移进 blocking 闭包，使调用方取消/超时后，
/// 仍在执行或已排入 blocking pool 的 KDF 保持占用配额，不能借取消绕过并发上限。
pub(crate) async fn derive_password_key(password: &str, salt: &[u8]) -> Result<PasswordKey> {
    ensure!(salt.len() == SALT_LEN, "认证盐必须为 {SALT_LEN} 字节");
    let permits = KDF_LIMITER
        .get_or_init(|| KdfLimiter::new(KDF_CONCURRENCY, KDF_QUEUE_CAPACITY))
        .acquire()
        .await?;
    // 只在获得计算配额后复制口令；临时副本和派生密钥在 drop 时清零。
    let password = Zeroizing::new(password.to_owned());
    let salt: [u8; SALT_LEN] = salt.try_into().expect("盐长度已验证");
    tokio::task::spawn_blocking(move || {
        let _permits = permits;
        let params = Params::new(
            ARGON_MEMORY_KIB,
            ARGON_ITERATIONS,
            ARGON_PARALLELISM,
            Some(KEY_LEN),
        )
        .map_err(|e| anyhow!("固定 Argon2id 参数无效: {e}"))?;
        let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
        let mut key = Zeroizing::new([0; KEY_LEN]);
        argon
            .hash_password_into(password.as_bytes(), &salt, key.as_mut())
            .map_err(|e| anyhow!("Argon2id 口令派生失败: {e}"))?;
        Ok(key)
    })
    .await
    .context("认证 KDF 后台任务失败")?
}

pub(crate) fn tls_exporter(conn: &Connection) -> Result<[u8; KEY_LEN]> {
    let mut exporter = [0; KEY_LEN];
    conn.export_keying_material(&mut exporter, EXPORTER_LABEL, b"")
        .map_err(|e| anyhow!("无法导出 TLS 认证通道绑定: {e:?}"))?;
    Ok(exporter)
}

/// LP(x) = u32BE(len(x)) || x；数字字段先编码成 u32BE，再作 LP。
/// 字段顺序冻结为 domain、auth version、完整 Hello（major/minor/name/pubkey/nonce/resume target）、
/// server nonce、salt、client role、server role、exporter。server_proof 不参与自身输入。
pub(crate) fn transcript(
    hello: &Hello,
    server_nonce: &[u8],
    salt: &[u8],
    auth_version: u32,
    exporter: &[u8; KEY_LEN],
) -> Vec<u8> {
    let mut out = Vec::new();
    for field in [
        b"DFYP-AUTH-V2".as_slice(),
        &auth_version.to_be_bytes(),
        &hello.proto_major.to_be_bytes(),
        &hello.proto_minor.to_be_bytes(),
        hello.client_name.as_bytes(),
        &hello.device_pubkey,
        &hello.client_nonce,
        &hello.resume_session_id,
        server_nonce,
        salt,
        b"client",
        b"server",
        exporter,
    ] {
        append_field(&mut out, field);
    }
    out
}

fn append_field(out: &mut Vec<u8>, field: &[u8]) {
    // 字段来自已受 MAX_FRAME_LEN 限制的 Hello，其他输入均固定长度。
    let len = u32::try_from(field.len()).expect("认证字段超过 u32 长度");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(field);
}

#[derive(Clone, Copy)]
pub(crate) enum ProofRole {
    Server,
    Client,
}

fn proof_mac(key: &PasswordKey, role: ProofRole, transcript: &[u8]) -> HmacSha256 {
    let mut input = Vec::new();
    append_field(
        &mut input,
        match role {
            ProofRole::Server => b"server-proof",
            ProofRole::Client => b"client-proof",
        },
    );
    append_field(&mut input, transcript);
    let mut mac = HmacSha256::new_from_slice(key.as_ref()).expect("HMAC 接受 32 字节密钥");
    mac.update(&input);
    mac
}

pub(crate) fn make_proof(key: &PasswordKey, role: ProofRole, transcript: &[u8]) -> Vec<u8> {
    proof_mac(key, role, transcript)
        .finalize()
        .into_bytes()
        .to_vec()
}

pub(crate) fn verify_proof(
    key: &PasswordKey,
    role: ProofRole,
    transcript: &[u8],
    proof: &[u8],
) -> bool {
    proof.len() == PROOF_LEN && proof_mac(key, role, transcript).verify_slice(proof).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;

    fn hello() -> Hello {
        Hello {
            proto_major: 1,
            proto_minor: 1,
            client_name: "test".into(),
            device_pubkey: vec![7; 32],
            client_nonce: vec![1; NONCE_LEN],
            resume_session_id: Vec::new(),
        }
    }

    #[test]
    fn transcript_binds_every_hello_field_and_both_roles() {
        let hello = hello();
        let key = Zeroizing::new([42; KEY_LEN]);
        let original = transcript(&hello, &[2; NONCE_LEN], &[3; SALT_LEN], 2, &[4; KEY_LEN]);
        let proof = make_proof(&key, ProofRole::Server, &original);
        assert!(verify_proof(&key, ProofRole::Server, &original, &proof));
        assert!(!verify_proof(&key, ProofRole::Client, &original, &proof));
        let mut variants = Vec::new();
        let mut changed = hello.clone();
        changed.proto_major += 1;
        variants.push(changed);
        let mut changed = hello.clone();
        changed.proto_minor += 1;
        variants.push(changed);
        let mut changed = hello.clone();
        changed.client_name.push('x');
        variants.push(changed);
        let mut changed = hello.clone();
        changed.device_pubkey[0] ^= 1;
        variants.push(changed);
        let mut changed = hello.clone();
        changed.client_nonce[0] ^= 1;
        variants.push(changed);
        let mut changed = hello.clone();
        changed.resume_session_id = vec![3; 16];
        variants.push(changed);
        for changed in variants {
            let changed = transcript(&changed, &[2; NONCE_LEN], &[3; SALT_LEN], 2, &[4; KEY_LEN]);
            assert!(!verify_proof(&key, ProofRole::Server, &changed, &proof));
        }
        for changed in [
            transcript(&hello, &[5; NONCE_LEN], &[3; SALT_LEN], 2, &[4; KEY_LEN]),
            transcript(&hello, &[2; NONCE_LEN], &[5; SALT_LEN], 2, &[4; KEY_LEN]),
            transcript(&hello, &[2; NONCE_LEN], &[3; SALT_LEN], 3, &[4; KEY_LEN]),
            transcript(&hello, &[2; NONCE_LEN], &[3; SALT_LEN], 2, &[5; KEY_LEN]),
        ] {
            assert!(!verify_proof(&key, ProofRole::Server, &changed, &proof));
        }
    }

    #[test]
    fn length_prefix_prevents_hello_field_concatenation_collisions() {
        let mut left = hello();
        left.client_name = "a".into();
        left.device_pubkey = b"bc".to_vec();
        let mut right = left.clone();
        right.client_name = "ab".into();
        right.device_pubkey = b"c".to_vec();
        assert_ne!(
            transcript(&left, &[2; NONCE_LEN], &[3; SALT_LEN], 2, &[4; KEY_LEN]),
            transcript(&right, &[2; NONCE_LEN], &[3; SALT_LEN], 2, &[4; KEY_LEN]),
        );
    }

    #[tokio::test]
    async fn kdf_admission_is_bounded_and_cancelled_waiter_releases_slot() {
        let limiter = Arc::new(KdfLimiter::new(1, 1));
        let running = limiter.acquire().await.unwrap();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let waiter_limiter = limiter.clone();
        let waiter = tokio::spawn(async move {
            // 将 acquire 手动 poll 一次，确保 admission 已占用而 active 尚在等待。
            let acquire = waiter_limiter.acquire();
            tokio::pin!(acquire);
            std::future::poll_fn(|cx| match acquire.as_mut().poll(cx) {
                std::task::Poll::Pending => std::task::Poll::Ready(()),
                std::task::Poll::Ready(_) => panic!("active permit 应仍被占用"),
            })
            .await;
            ready_tx.send(()).unwrap();
            acquire.await
        });
        ready_rx.await.unwrap();
        assert!(limiter.acquire().await.is_err());
        waiter.abort();
        assert!(matches!(waiter.await, Err(error) if error.is_cancelled()));
        assert_eq!(limiter.admission.available_permits(), 1);
        drop(running);
        let _next = limiter.acquire().await.unwrap();
    }

    #[test]
    fn cancelled_caller_does_not_release_queued_blocking_kdf_permits() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocker = runtime.spawn_blocking(move || {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        started_rx.recv().unwrap();
        runtime.block_on(async {
            let kdf = derive_password_key("cancellation-regression", &[9; SALT_LEN]);
            tokio::pin!(kdf);
            // blocking pool 唯一线程已占用；此时真实 KDF 只能排队，不能完成。
            std::future::poll_fn(|cx| match kdf.as_mut().poll(cx) {
                std::task::Poll::Pending => std::task::Poll::Ready(()),
                std::task::Poll::Ready(_) => panic!("KDF 必须等待 blocking pool"),
            })
            .await;
        }); // 丢弃调用 future，模拟握手取消或超时。
        let limiter = KDF_LIMITER.get().unwrap();
        assert_eq!(limiter.active.available_permits(), KDF_CONCURRENCY - 1);
        assert_eq!(
            limiter.admission.available_permits(),
            KDF_CONCURRENCY + KDF_QUEUE_CAPACITY - 1
        );
        release_tx.send(()).unwrap();
        runtime.block_on(async {
            blocker.await.unwrap();
            let _all_active = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                limiter
                    .active
                    .clone()
                    .acquire_many_owned(KDF_CONCURRENCY as u32),
            )
            .await
            .expect("被取消调用的后台 KDF 必须最终结束")
            .unwrap();
            assert_eq!(
                limiter.admission.available_permits(),
                KDF_CONCURRENCY + KDF_QUEUE_CAPACITY
            );
        });
    }
}
