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

/// 共享的 `Arc<Endpoint>`（预留给后续多连接/重连场景）。
pub fn shared(endpoint: Endpoint) -> Arc<Endpoint> {
    Arc::new(endpoint)
}
