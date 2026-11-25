//! 应用层握手：`Hello → Challenge → Auth → Welcome → CapabilityAck`。
//!
//! 与 `spec/DFYP-v1.md` 的握手章节对应；MVP 简化点集中在证明算法（见 [`crate::util`]）。

use std::time::Duration;

use anyhow::{bail, Context, Result};
use dfy_proto::{
    envelope, error_name, Envelope, Welcome, CAPABILITIES, ERR_AUTH, ERR_PROTOCOL, ERR_VERSION,
    PERM_DEFAULT, PROTO_MAJOR, PROTO_MINOR,
};
use quinn::{Connection, VarInt};
use tracing::{debug, warn};

use crate::{
    ctrl::CtrlChannel,
    util::{password_proof, random_bytes, verify_proof},
    HANDSHAKE_TIMEOUT,
};

/// 服务端握手策略（口令、Agent 名、授予的能力/权限）。
#[derive(Clone, Debug)]
pub struct ServerAuth {
    /// 连接口令（MVP 明文比较用的是 HMAC 证明，不传输口令本身）。
    pub password: String,
    /// Agent 显示名。
    pub agent_name: String,
    /// 声明支持的能力。
    pub capabilities: Vec<String>,
    /// 授予的权限位（默认 `VIEW|CONTROL|CLIPBOARD`）。
    pub permission_bitmap: u32,
}

impl ServerAuth {
    /// 默认授予 `VIEW|CONTROL|CLIPBOARD`，能力清单取 `dfy_proto::CAPABILITIES`。
    pub fn new(password: impl Into<String>, agent_name: impl Into<String>) -> Self {
        Self {
            password: password.into(),
            agent_name: agent_name.into(),
            capabilities: CAPABILITIES.iter().map(|c| (*c).to_string()).collect(),
            permission_bitmap: PERM_DEFAULT,
        }
    }
}

/// Agent 侧：接受 control 流并完成握手；返回 `Welcome` 与已建立的 control 通道。
pub async fn server_handshake(
    conn: &Connection,
    auth: &ServerAuth,
) -> Result<(Welcome, CtrlChannel)> {
    tokio::time::timeout(HANDSHAKE_TIMEOUT, server_handshake_inner(conn, auth))
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "服务端握手超时（{}s）：未收到客户端完整的 Hello/Auth",
                HANDSHAKE_TIMEOUT.as_secs()
            )
        })?
}

async fn server_handshake_inner(
    conn: &Connection,
    auth: &ServerAuth,
) -> Result<(Welcome, CtrlChannel)> {
    let (send, recv) = conn
        .accept_bi()
        .await
        .context("等待客户端打开 control 双向流失败")?;
    let mut chan = CtrlChannel::new(send, recv, conn.clone());

    // 1) Hello
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
    if hello.proto_major != PROTO_MAJOR {
        let _ = chan.send_ctrl(&Envelope::reject(ERR_VERSION)).await;
        bail!(
            "协议主版本不兼容：客户端 {}.{}, 本端 {}.{}",
            hello.proto_major,
            hello.proto_minor,
            PROTO_MAJOR,
            PROTO_MINOR
        );
    }

    // 2) Challenge（32B 随机 nonce）
    let nonce = random_bytes(32);
    chan.send_ctrl(&Envelope::challenge(nonce.clone()))
        .await
        .context("发送 Challenge 失败")?;

    // 3) Auth
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
    if !verify_proof(&auth.password, &nonce, &proof) {
        // 认证失败：先回 Reject，给对端一点时间读完这一帧再断开，避免客户端只看到
        // "connection closed" 而拿不到具体的错误码（改善诊断体验）。
        let _ = chan.send_ctrl(&Envelope::reject(ERR_AUTH)).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        conn.close(VarInt::from_u32(ERR_AUTH), b"auth rejected");
        bail!("认证失败：口令不匹配（客户端 proof 校验未通过）");
    }

    // 4) Welcome + CapabilityAck
    let session_id = random_bytes(16);
    let welcome = Welcome {
        session_id: session_id.clone(),
        agent_name: auth.agent_name.clone(),
        capability: auth.capabilities.clone(),
        permission_bitmap: auth.permission_bitmap,
    };
    chan.send_ctrl(&Envelope::welcome(
        session_id,
        auth.agent_name.clone(),
        auth.capabilities.clone(),
        auth.permission_bitmap,
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

    Ok((welcome, chan))
}

/// Client 侧：打开 control 流并完成握手。
pub async fn client_handshake(
    conn: &Connection,
    password: &str,
    client_name: &str,
    device_pubkey: Vec<u8>,
) -> Result<(Welcome, CtrlChannel)> {
    tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        client_handshake_inner(conn, password, client_name, device_pubkey),
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
) -> Result<(Welcome, CtrlChannel)> {
    let (send, recv) = conn
        .open_bi()
        .await
        .context("打开 control 双向流失败（连接已断开？）")?;
    let mut chan = CtrlChannel::new(send, recv, conn.clone());

    // 1) Hello
    chan.send_ctrl(&Envelope::hello(
        PROTO_MAJOR,
        PROTO_MINOR,
        client_name,
        device_pubkey,
    ))
    .await
    .context("发送 Hello 失败")?;

    // 2) Challenge
    let challenge_env = chan
        .recv_ctrl()
        .await?
        .context("握手失败：服务端在 Challenge 之前关闭了 control 流")?;
    let nonce = match challenge_env.body {
        Some(envelope::Body::Challenge(challenge)) => challenge.nonce,
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

    // 3) Auth
    let proof = password_proof(password, &nonce);
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

    Ok((welcome, chan))
}

/// 握手成功后 Agent 常发的初始配置帧（视频参数 + 能力确认）。
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
