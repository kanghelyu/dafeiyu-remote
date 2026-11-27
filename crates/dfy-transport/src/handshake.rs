//! 应用层握手：`Hello → Challenge(server proof) → Auth(client proof) → Welcome → CapabilityAck`。
//!
//! 认证 v2 使用固定 Argon2id 参数和 TLS exporter，证明绑定完整 Hello、双方 nonce、
//! salt 和角色。不支持旧 nonce-only 模式；该口令互证不是 PAKE 或持久设备配对。

use std::{fmt, time::Duration};

use anyhow::{bail, ensure, Context, Result};
use dfy_proto::{
    envelope, error_name, Challenge, Envelope, Hello, SessionResume, SessionResumeOk, Welcome,
    CAPABILITIES, ERR_AUTH, ERR_BUSY, ERR_PROTOCOL, ERR_VERSION, PERM_DEFAULT, PROTO_MAJOR,
    PROTO_MINOR, SESSION_ID_LEN, SESSION_RESUME_COMMIT, SESSION_RESUME_PREPARE,
};
use ed25519_dalek::SigningKey;
use quinn::{Connection, VarInt};
use tracing::{debug, warn};
use zeroize::Zeroizing;

use crate::{
    auth::{
        derive_password_key, make_proof, tls_exporter, transcript, verify_proof, ProofRole,
        AUTH_MIN_PROTO_MINOR, AUTH_VERSION, NONCE_LEN, PROOF_LEN, SALT_LEN,
    },
    ctrl::CtrlChannel,
    resume::{sign_request, ResumeGrant, ResumeRegistry},
    util::random_bytes,
    HANDSHAKE_TIMEOUT,
};

/// 服务端握手策略（口令、Agent 名、授予的能力/权限）。
#[derive(Clone)]
pub struct ServerAuth {
    /// 连接口令（仅在内存参与 Argon2id 派生，不在协议中发送或 Debug 中输出）。
    pub password: Zeroizing<String>,
    /// Agent 显示名。
    pub agent_name: String,
    /// 声明支持的能力。
    pub capabilities: Vec<String>,
    /// 授予的权限位（默认仅 `VIEW`）。
    pub permission_bitmap: u32,
    /// 跨连接保存的一次性会话恢复授权。
    pub resume_registry: ResumeRegistry,
}

impl fmt::Debug for ServerAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerAuth")
            .field("password", &"[REDACTED]")
            .field("agent_name", &self.agent_name)
            .field("capabilities", &self.capabilities)
            .field("permission_bitmap", &self.permission_bitmap)
            .field("resume_registry", &"[PRIVATE]")
            .finish()
    }
}

impl ServerAuth {
    /// 默认仅授予 `VIEW`，能力清单取 `dfy_proto::CAPABILITIES`。
    pub fn new(password: impl AsRef<str>, agent_name: impl Into<String>) -> Self {
        Self {
            password: Zeroizing::new(password.as_ref().to_owned()),
            agent_name: agent_name.into(),
            capabilities: CAPABILITIES.iter().map(|c| (*c).to_string()).collect(),
            permission_bitmap: PERM_DEFAULT,
            resume_registry: ResumeRegistry::default(),
        }
    }
}

pub struct ServerHandshake {
    pub welcome: Welcome,
    pub chan: CtrlChannel,
    pub hello: Hello,
    pub exporter: [u8; 32],
}

/// Agent 侧：接受 control 流并完成握手；返回 `Welcome` 与已建立的 control 通道。
pub async fn server_handshake(
    conn: &Connection,
    auth: &ServerAuth,
) -> Result<(Welcome, CtrlChannel)> {
    let context = server_handshake_context(conn, auth).await?;
    Ok((context.welcome, context.chan))
}

pub async fn server_handshake_context(
    conn: &Connection,
    auth: &ServerAuth,
) -> Result<ServerHandshake> {
    tokio::time::timeout(HANDSHAKE_TIMEOUT, server_handshake_inner(conn, auth))
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "服务端握手超时（{}s）：未收到客户端完整的 Hello/Auth",
                HANDSHAKE_TIMEOUT.as_secs()
            )
        })?
}

async fn server_handshake_inner(conn: &Connection, auth: &ServerAuth) -> Result<ServerHandshake> {
    let (send, recv) = conn
        .accept_bi()
        .await
        .context("等待客户端打开 control 双向流失败")?;
    let mut chan = CtrlChannel::new(send, recv, conn.clone());

    // 1) Hello：没有 client nonce 或旧版本的客户端不能降级认证。
    let hello_env = chan
        .recv_ctrl()
        .await?
        .context("握手失败：客户端未发送 Hello 就关闭了 control 流")?;
    let hello = match hello_env.body {
        Some(envelope::Body::Hello(hello)) => hello,
        Some(_) => {
            let _ = chan.send_ctrl(&Envelope::reject(ERR_PROTOCOL)).await;
            bail!("握手第一帧必须是 Hello，实际收到 {}", hello_env.kind_name());
        }
        None => bail!("握手失败：收到空 Envelope"),
    };
    debug!(
        client = %hello.client_name,
        proto = format!("{}.{}", hello.proto_major, hello.proto_minor),
        pubkey_len = hello.device_pubkey.len(),
        "收到 Hello"
    );
    if hello.proto_major != PROTO_MAJOR || hello.proto_minor < AUTH_MIN_PROTO_MINOR {
        let _ = chan.send_ctrl(&Envelope::reject(ERR_VERSION)).await;
        bail!(
            "协议版本不支持认证 v2：客户端 {}.{}, 本端 {}.{}",
            hello.proto_major,
            hello.proto_minor,
            PROTO_MAJOR,
            PROTO_MINOR
        );
    }
    if hello.client_nonce.len() != NONCE_LEN {
        let _ = chan.send_ctrl(&Envelope::reject(ERR_PROTOCOL)).await;
        bail!("认证 v2 要求 Hello.client_nonce 为 {NONCE_LEN} 字节");
    }
    if !hello.resume_session_id.is_empty()
        && hello.resume_session_id.len() != dfy_proto::SESSION_ID_LEN
    {
        let _ = chan.send_ctrl(&Envelope::reject(ERR_PROTOCOL)).await;
        bail!("Hello.resume_session_id 必须为空或为 16B");
    }

    // 2) Challenge：先向客户端证明服务端持有口令，且绑定这条 TLS 会话。
    let nonce = random_bytes(NONCE_LEN);
    let salt = random_bytes(SALT_LEN);
    let exporter = tls_exporter(conn)?;
    let transcript = transcript(&hello, &nonce, &salt, AUTH_VERSION, &exporter);
    let key = match derive_password_key(&auth.password, &salt).await {
        Ok(key) => key,
        Err(error) => {
            let _ = chan.send_ctrl(&Envelope::reject(ERR_BUSY)).await;
            return Err(error.context("服务端认证密钥派生失败"));
        }
    };
    let server_proof = make_proof(&key, ProofRole::Server, &transcript);
    chan.send_ctrl(&Envelope {
        body: Some(envelope::Body::Challenge(Challenge {
            nonce,
            salt,
            server_proof,
            auth_version: AUTH_VERSION,
        })),
    })
    .await
    .context("发送 Challenge 失败")?;

    // 3) Auth：client-proof 与 server-proof 使用不同的 HMAC 域，不能反射。
    let auth_env = chan
        .recv_ctrl()
        .await?
        .context("握手失败：客户端未发送 Auth 就关闭了 control 流")?;
    let proof = match auth_env.body {
        Some(envelope::Body::Auth(auth_msg)) => auth_msg.proof,
        Some(_) => {
            let _ = chan.send_ctrl(&Envelope::reject(ERR_PROTOCOL)).await;
            bail!("握手第二帧必须是 Auth，实际收到 {}", auth_env.kind_name());
        }
        None => bail!("握手失败：收到空 Envelope"),
    };
    if !verify_proof(&key, ProofRole::Client, &transcript, &proof) {
        // 留出读取 Reject 的时间，使调用方可获得具体错误码。
        let _ = chan.send_ctrl(&Envelope::reject(ERR_AUTH)).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        conn.close(VarInt::from_u32(ERR_AUTH), b"auth rejected");
        bail!("认证失败：客户端口令或通道绑定证明校验未通过");
    }
    drop(key);

    // 4) Welcome + CapabilityAck
    let session_id = random_bytes(SESSION_ID_LEN);
    let grant = if hello.device_pubkey.len() == 32 {
        Some(
            auth.resume_registry.new_grant(ResumeGrant {
                session_id: session_id.clone(),
                session_epoch: 0,
                device_pubkey: hello
                    .device_pubkey
                    .clone()
                    .try_into()
                    .expect("设备公钥长度已验证"),
                permission_bitmap: auth.permission_bitmap,
                capability: auth.capabilities.clone(),
                resume_token: Vec::new(),
            })?,
        )
    } else {
        None
    };
    let welcome = Welcome {
        session_id: session_id.clone(),
        agent_name: auth.agent_name.clone(),
        capability: auth.capabilities.clone(),
        permission_bitmap: auth.permission_bitmap,
        resume_token: grant
            .as_ref()
            .map(|grant| grant.resume_token.clone())
            .unwrap_or_default(),
        session_epoch: grant
            .as_ref()
            .map(|grant| grant.session_epoch)
            .unwrap_or_default(),
    };
    chan.send_ctrl(&Envelope::welcome_with_resume(
        session_id,
        auth.agent_name.clone(),
        auth.capabilities.clone(),
        auth.permission_bitmap,
        welcome.resume_token.clone(),
        welcome.session_epoch,
    ))
    .await
    .context("发送 Welcome 失败")?;
    chan.send_ctrl(&Envelope::capability_ack())
        .await
        .context("发送 CapabilityAck 失败")?;

    // 5) 等客户端 CapabilityAck（3s 容忍；迟到也算成功，后续消息由业务层处理）
    match tokio::time::timeout(Duration::from_secs(3), chan.recv_ctrl()).await {
        Ok(Ok(Some(env))) => {
            if !matches!(env.body, Some(envelope::Body::CapabilityAck(_))) {
                warn!(kind = env.kind_name(), "握手完成阶段收到意外消息（已忽略）");
            }
        }
        Ok(Ok(None)) => warn!("客户端在 CapabilityAck 之前关闭 control 流"),
        Ok(Err(e)) => return Err(e.context("等待客户端 CapabilityAck 失败")),
        Err(_) => warn!("等待客户端 CapabilityAck 超时（3s），继续"),
    }

    Ok(ServerHandshake {
        welcome,
        chan,
        hello,
        exporter,
    })
}

pub struct ClientHandshake {
    pub welcome: Welcome,
    pub chan: CtrlChannel,
    pub hello: Hello,
    pub exporter: [u8; 32],
}

/// Client 侧：打开 control 流并完成握手；验证 server-proof 前不发送 client-proof。
pub async fn client_handshake(
    conn: &Connection,
    password: &str,
    client_name: &str,
    device_pubkey: Vec<u8>,
) -> Result<(Welcome, CtrlChannel)> {
    let context = client_handshake_context(conn, password, client_name, device_pubkey).await?;
    Ok((context.welcome, context.chan))
}

pub async fn client_handshake_context(
    conn: &Connection,
    password: &str,
    client_name: &str,
    device_pubkey: Vec<u8>,
) -> Result<ClientHandshake> {
    client_handshake_context_for_session(conn, password, client_name, device_pubkey, Vec::new())
        .await
}

pub async fn client_handshake_context_for_session(
    conn: &Connection,
    password: &str,
    client_name: &str,
    device_pubkey: Vec<u8>,
    resume_session_id: Vec<u8>,
) -> Result<ClientHandshake> {
    tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        client_handshake_inner(
            conn,
            password,
            client_name,
            device_pubkey,
            resume_session_id,
        ),
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "客户端握手超时（{}s）：未收到 Challenge/Welcome",
            HANDSHAKE_TIMEOUT.as_secs()
        )
    })?
}

async fn client_handshake_inner(
    conn: &Connection,
    password: &str,
    client_name: &str,
    device_pubkey: Vec<u8>,
    resume_session_id: Vec<u8>,
) -> Result<ClientHandshake> {
    let (send, recv) = conn
        .open_bi()
        .await
        .context("打开 control 双向流失败（连接已断开？）")?;
    let mut chan = CtrlChannel::new(send, recv, conn.clone());

    // 1) Hello：客户端也提供新随机数，完整声明纳入双方证明。
    let hello = Hello {
        proto_major: PROTO_MAJOR,
        proto_minor: PROTO_MINOR,
        client_name: client_name.to_owned(),
        device_pubkey,
        client_nonce: random_bytes(NONCE_LEN),
        resume_session_id,
    };
    chan.send_ctrl(&Envelope {
        body: Some(envelope::Body::Hello(hello.clone())),
    })
    .await
    .context("发送 Hello 失败")?;

    // 2) Challenge：严格要求认证 v2，不接受空字段/旧 nonce-only 形式。
    let challenge_env = chan
        .recv_ctrl()
        .await?
        .context("握手失败：服务端在 Challenge 之前关闭了 control 流")?;
    let challenge = match challenge_env.body {
        Some(envelope::Body::Challenge(challenge)) => challenge,
        Some(envelope::Body::Reject(reject)) => bail!(
            "服务端拒绝握手: {} (code={})",
            error_name(reject.code),
            reject.code
        ),
        Some(_) => bail!(
            "握手第二帧必须是 Challenge，实际收到 {}",
            challenge_env.kind_name()
        ),
        None => bail!("握手失败：收到空 Envelope"),
    };
    if challenge.auth_version != AUTH_VERSION {
        conn.close(VarInt::from_u32(ERR_VERSION), b"auth v2 required");
        bail!("认证版本不兼容：必须为 v2，禁止回退旧 nonce-only 认证");
    }
    if challenge.nonce.len() != NONCE_LEN
        || challenge.salt.len() != SALT_LEN
        || challenge.server_proof.len() != PROOF_LEN
    {
        conn.close(VarInt::from_u32(ERR_PROTOCOL), b"invalid auth challenge");
        bail!("认证 v2 Challenge 字段长度非法（nonce32/salt16/server_proof32）");
    }
    let exporter = tls_exporter(conn)?;
    let transcript = transcript(
        &hello,
        &challenge.nonce,
        &challenge.salt,
        challenge.auth_version,
        &exporter,
    );
    let key = derive_password_key(password, &challenge.salt)
        .await
        .context("客户端认证密钥派生失败")?;
    if !verify_proof(
        &key,
        ProofRole::Server,
        &transcript,
        &challenge.server_proof,
    ) {
        conn.close(VarInt::from_u32(ERR_AUTH), b"server proof rejected");
        bail!("认证失败：服务端口令或通道绑定证明校验未通过");
    }

    // 3) Auth：只有确认服务端证明后才暴露客户端证明。
    let proof = make_proof(&key, ProofRole::Client, &transcript);
    drop(key);
    chan.send_ctrl(&Envelope::auth(proof))
        .await
        .context("发送 Auth 失败")?;

    // 4) Welcome
    let welcome_env = chan
        .recv_ctrl()
        .await?
        .context("握手失败：服务端在 Welcome 之前关闭了 control 流")?;
    let welcome = match welcome_env.body {
        Some(envelope::Body::Welcome(welcome)) => welcome,
        Some(envelope::Body::Reject(reject)) => bail!(
            "服务端拒绝握手: {} (code={}) —— 口令错误或版本不兼容",
            error_name(reject.code),
            reject.code
        ),
        Some(_) => bail!(
            "握手第四帧必须是 Welcome，实际收到 {}",
            welcome_env.kind_name()
        ),
        None => bail!("握手失败：收到空 Envelope"),
    };
    debug!(
        session_id = hex::encode(&welcome.session_id),
        agent = %welcome.agent_name,
        permission_bitmap = format!("0x{:04x}", welcome.permission_bitmap),
        capabilities = welcome.capability.len(),
        "握手成功，收到 Welcome"
    );

    // 5) CapabilityAck 双向确认
    chan.send_ctrl(&Envelope::capability_ack())
        .await
        .context("发送 CapabilityAck 失败")?;
    match tokio::time::timeout(Duration::from_secs(3), chan.recv_ctrl()).await {
        Ok(Ok(Some(env))) => {
            if !matches!(env.body, Some(envelope::Body::CapabilityAck(_))) {
                warn!(kind = env.kind_name(), "握手完成阶段收到意外消息（已忽略）");
            }
        }
        Ok(Ok(None)) => warn!("服务端在 CapabilityAck 之前关闭 control 流"),
        Ok(Err(e)) => return Err(e.context("等待服务端 CapabilityAck 失败")),
        Err(_) => warn!("等待服务端 CapabilityAck 超时（3s），继续"),
    }

    Ok(ClientHandshake {
        welcome,
        chan,
        hello,
        exporter,
    })
}

/// 在新路径上完成口令认证后，用原会话令牌执行 PREPARE/COMMIT。
pub async fn client_resume_handshake(
    conn: &Connection,
    password: &str,
    client_name: &str,
    signing_key: &SigningKey,
    previous: &Welcome,
    last_frame_id: u64,
    last_frame_seq: u64,
) -> Result<(Welcome, CtrlChannel)> {
    ensure_resume_welcome(previous)?;
    let context = client_handshake_context_for_session(
        conn,
        password,
        client_name,
        signing_key.verifying_key().to_bytes().to_vec(),
        previous.session_id.clone(),
    )
    .await?;
    let mut chan = context.chan;
    let mut prepare = SessionResume {
        last_frame_id,
        last_frame_seq,
        session_id: previous.session_id.clone(),
        resume_token: previous.resume_token.clone(),
        phase: SESSION_RESUME_PREPARE,
        client_nonce: random_bytes(dfy_proto::RESUME_NONCE_LEN),
        server_nonce: Vec::new(),
        signature: Vec::new(),
        permission_bitmap: previous.permission_bitmap,
    };
    prepare.signature = sign_request(signing_key, &context.hello, &context.exporter, &prepare)?;
    chan.send_ctrl(&Envelope {
        body: Some(envelope::Body::SessionResume(prepare.clone())),
    })
    .await
    .context("发送迁移 PREPARE 失败")?;
    let prepared = recv_resume_ok(&mut chan, SESSION_RESUME_PREPARE, &prepare.session_id).await?;
    ensure!(
        prepared.server_nonce.len() == dfy_proto::RESUME_NONCE_LEN,
        "PREPARE 响应随机数长度非法"
    );
    ensure!(
        prepared.resume_token == prepare.resume_token,
        "PREPARE 响应令牌不匹配"
    );
    ensure!(
        prepared.permission_bitmap == prepare.permission_bitmap
            && prepared.capability == previous.capability,
        "PREPARE 响应权限或能力发生变化"
    );
    let mut commit = prepare.clone();
    commit.phase = SESSION_RESUME_COMMIT;
    commit.server_nonce = prepared.server_nonce;
    commit.signature = sign_request(signing_key, &context.hello, &context.exporter, &commit)?;
    chan.send_ctrl(&Envelope {
        body: Some(envelope::Body::SessionResume(commit.clone())),
    })
    .await
    .context("发送迁移 COMMIT 失败")?;
    let committed = recv_resume_ok(&mut chan, SESSION_RESUME_COMMIT, &commit.session_id).await?;
    ensure!(
        committed.session_epoch
            == previous
                .session_epoch
                .checked_add(1)
                .context("会话代数溢出")?,
        "COMMIT 会话代数不连续"
    );
    ensure!(
        committed.server_nonce == commit.server_nonce,
        "COMMIT 响应随机数不匹配"
    );
    ensure!(
        committed.resume_token.len() == dfy_proto::RESUME_TOKEN_LEN
            && committed.resume_token != commit.resume_token,
        "COMMIT 未轮换恢复令牌"
    );
    ensure!(
        committed.permission_bitmap == commit.permission_bitmap
            && committed.capability == previous.capability,
        "COMMIT 响应权限或能力发生变化"
    );
    Ok((
        Welcome {
            session_id: committed.session_id,
            agent_name: previous.agent_name.clone(),
            capability: committed.capability,
            permission_bitmap: committed.permission_bitmap,
            resume_token: committed.resume_token,
            session_epoch: committed.session_epoch,
        },
        chan,
    ))
}

async fn recv_resume_ok(
    chan: &mut CtrlChannel,
    phase: u32,
    session_id: &[u8],
) -> Result<SessionResumeOk> {
    loop {
        let env = chan
            .recv_ctrl()
            .await?
            .context("迁移确认前 control 流关闭")?;
        match env.body {
            Some(envelope::Body::Ping(ping)) => {
                chan.send_ctrl(&Envelope::pong(ping.ts_us))
                    .await
                    .context("迁移期间回复 Pong 失败")?;
            }
            Some(envelope::Body::Pong(_)) => {}
            Some(envelope::Body::SessionResumeOk(ok)) => {
                ensure!(ok.phase == phase, "迁移确认阶段不匹配");
                ensure!(ok.session_id == session_id, "迁移确认会话不匹配");
                return Ok(ok);
            }
            Some(envelope::Body::Reject(reject)) => bail!(
                "服务端拒绝迁移: {} (code={})",
                error_name(reject.code),
                reject.code
            ),
            _ => bail!("预期迁移确认，实际收到 {}", env.kind_name()),
        }
    }
}

fn ensure_resume_welcome(welcome: &Welcome) -> Result<()> {
    anyhow::ensure!(
        welcome.session_id.len() == SESSION_ID_LEN,
        "原会话 ID 长度非法"
    );
    anyhow::ensure!(
        welcome.resume_token.len() == dfy_proto::RESUME_TOKEN_LEN,
        "原会话不支持安全迁移"
    );
    Ok(())
}

/// 处理已认证连接上的迁移请求；调用方负责将回复发送给对端。
pub fn server_resume_request(
    auth: &ServerAuth,
    hello: &Hello,
    exporter: &[u8; 32],
    request: &SessionResume,
    resume_from_frame_id: u64,
) -> Result<SessionResumeOk> {
    match request.phase {
        SESSION_RESUME_PREPARE => {
            let prepared = auth.resume_registry.prepare(hello, exporter, request)?;
            Ok(SessionResumeOk {
                keyframe_requested: false,
                resume_from_frame_id,
                session_id: prepared.grant.session_id,
                session_epoch: prepared.grant.session_epoch,
                phase: SESSION_RESUME_PREPARE,
                server_nonce: prepared.server_nonce.to_vec(),
                resume_token: prepared.grant.resume_token,
                permission_bitmap: prepared.grant.permission_bitmap,
                capability: prepared.grant.capability,
            })
        }
        SESSION_RESUME_COMMIT => {
            let committed = auth.resume_registry.commit(hello, exporter, request)?;
            Ok(SessionResumeOk {
                keyframe_requested: true,
                resume_from_frame_id,
                session_id: committed.grant.session_id,
                session_epoch: committed.grant.session_epoch,
                phase: SESSION_RESUME_COMMIT,
                server_nonce: committed.server_nonce.to_vec(),
                resume_token: committed.grant.resume_token,
                permission_bitmap: committed.grant.permission_bitmap,
                capability: committed.grant.capability,
            })
        }
        _ => bail!("未知迁移阶段 {}", request.phase),
    }
}
pub fn initial_config_frames(
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
) -> Vec<Envelope> {
    vec![
        Envelope::video_config(width, height, fps, bitrate_kbps),
        Envelope::capability_ack(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_auth_debug_redacts_password_and_defaults_to_view() {
        let auth = ServerAuth::new("private-password-must-not-appear", "test-agent");
        let debug = format!("{auth:?}");
        assert!(!debug.contains(auth.password.as_str()));
        assert!(debug.contains("[REDACTED]"));
        assert_eq!(auth.permission_bitmap, dfy_proto::PERM_VIEW);
    }
}
