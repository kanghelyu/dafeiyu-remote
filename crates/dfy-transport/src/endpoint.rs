//! Endpoint 绑定与连接建立（Agent 监听 / Client 拨号）。

use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::Arc,
};

use anyhow::{Context, Result};
use dfy_proto::ALPN;
use quinn::{Connection, Endpoint};
use tracing::{debug, info};

use crate::tls::{self, ClientTls, TlsIdentity, SERVER_NAME};

/// 绑定 Agent 的 QUIC 监听端点（UDP），同时生成一次性自签身份。
pub fn bind_agent(addr: SocketAddr) -> Result<(Endpoint, TlsIdentity)> {
    let identity = TlsIdentity::generate("dfy-agent").context("生成 Agent TLS 身份失败")?;
    bind_agent_with_identity(addr, identity)
}

/// 使用调用方持久化或恢复的身份绑定 Agent 端点。
pub fn bind_agent_with_identity(
    addr: SocketAddr,
    identity: TlsIdentity,
) -> Result<(Endpoint, TlsIdentity)> {
    let server_cfg = tls::server_config(&identity)?;
    let endpoint = Endpoint::server(server_cfg, addr)
        .with_context(|| format!("Agent QUIC 端点绑定失败（UDP {addr}）"))?;
    info!(%addr, fingerprint = %identity.fingerprint_hex(), "Agent QUIC 端点已就绪");
    Ok((endpoint, identity))
}

/// 新建客户端端点（本地随机端口）并拨号。
///
/// 返回的 [`Endpoint`] 需要调用方持有：虽然 quinn 的连接会在端点驱动存活期间保持，
/// 但把句柄留在调用方更直观，也便于优雅关闭。
pub async fn connect(addr: SocketAddr, tls: ClientTls) -> Result<(Endpoint, Connection)> {
    let bind_addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0));
    let mut endpoint = Endpoint::client(bind_addr).context("创建客户端 QUIC 端点失败")?;
    endpoint.set_default_client_config(tls.config.clone());
    debug!(%addr, alpn = std::str::from_utf8(ALPN).unwrap_or("dfy"), "拨号中");
    let connecting = endpoint
        .connect(addr, SERVER_NAME)
        .with_context(|| format!("发起 QUIC 连接失败（{addr}）"))?;
    let conn = connecting
        .await
        .with_context(|| format!("QUIC/TLS 连接失败（{addr}）"))?;
    let observed = tls
        .observed_hex()
        .unwrap_or_else(|| "<unknown>".to_string());
    info!(%addr, fingerprint = %observed, "QUIC 连接已建立");
    Ok((endpoint, conn))
}

/// 便捷封装：`Endpoint::server` 但直接拿到监听地址（`port = 0` 时用于自测）。
pub fn bind_agent_random() -> Result<(Endpoint, TlsIdentity, SocketAddr)> {
    let (endpoint, identity) =
        bind_agent(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)))?;
    let addr = endpoint.local_addr().context("读取 Agent 监听地址失败")?;
    Ok((endpoint, identity, addr))
}

/// 中继模式：用调用方提供的 UDP socket（已完成 tag 配对）承载 QUIC 服务端。
///
/// 中继按 tag 成对转发字节，QUIC 端点看到的对端始终是中继地址，因此 TLS/连接语义不变。
/// `identity` 由调用方生成（与 `bind_agent` 等价），跨会话保持不变才能让客户端校验指纹。
pub fn agent_endpoint_on_socket(
    socket: std::net::UdpSocket,
    identity: TlsIdentity,
) -> Result<Endpoint> {
    let server_cfg = tls::server_config(&identity)?;
    let endpoint = Endpoint::new(
        quinn::EndpointConfig::default(),
        Some(server_cfg),
        socket,
        Arc::new(quinn::TokioRuntime),
    )
    .context("用中继 socket 创建 Agent QUIC 端点失败")?;
    info!(
        fingerprint = %identity.fingerprint_hex(),
        "Agent QUIC 端点已就绪（经中继）"
    );
    Ok(endpoint)
}

/// 中继模式：用调用方提供的 UDP socket 拨号到 `peer`（= 中继地址）。
pub async fn connect_on_socket(
    socket: std::net::UdpSocket,
    tls: ClientTls,
    peer: SocketAddr,
) -> Result<(Endpoint, Connection)> {
    let mut endpoint = Endpoint::new(
        quinn::EndpointConfig::default(),
        None,
        socket,
        Arc::new(quinn::TokioRuntime),
    )
    .context("用中继 socket 创建客户端 QUIC 端点失败")?;
    endpoint.set_default_client_config(tls.config.clone());
    debug!(%peer, "经中继拨号");
    let connecting = endpoint
        .connect(peer, SERVER_NAME)
        .with_context(|| format!("经中继发起 QUIC 连接失败（{peer}）"))?;
    let conn = connecting
        .await
        .with_context(|| format!("经中继 QUIC/TLS 连接失败（{peer}）"))?;
    let observed = tls
        .observed_hex()
        .unwrap_or_else(|| "<unknown>".to_string());
    info!(%peer, fingerprint = %observed, "经中继的 QUIC 连接已建立");
    Ok((endpoint, conn))
}

/// 共享的 `Arc<Endpoint>`（预留给后续多连接/重连场景）。
pub fn shared(endpoint: Endpoint) -> Arc<Endpoint> {
    Arc::new(endpoint)
}
