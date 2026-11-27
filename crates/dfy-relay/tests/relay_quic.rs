// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 中继客户端（[`dfy_relay::RelaySocket`]）集成测试。
//!
//! 覆盖两件事：
//! 1. 同一进程内起一个真实中继（127.0.0.1:0），两个 role 的 relay socket 完成
//!    bind → 双向 UDP 转发 → 保活 BindRequest 幂等（不产生 rebind）；
//! 2. **QUIC over relay**：两个 quinn endpoint 各自复用 relay socket（dup 同端口），
//!    Client `connect(中继地址)` → 完整握手 → 10 帧 1KB 视频 DATAGRAM → Ping/Pong。
//!
//! 若第 2 项在某个平台跑不通，保留第 1 项即可验证 relay 客户端 socket 的双向收发
//! （这也是 README「失败模式与回退」里描述的降级验证）。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use dfy_relay::config::{Config, Limits, LogLevel, RelayKey};
use dfy_relay::metrics::DropReason;
use dfy_relay::proto::{self, Tag};
use dfy_relay::relay::{self, RelayHandle};
use dfy_relay::{bind_relay_socket_with, RelayClientConfig, RelaySocketOptions, Role};
use dfy_transport::{
    client_handshake, client_tls, send_video_frame, server_config, server_handshake, AgentSession,
    ClientSession, ServerAuth, TlsIdentity, VideoReceiver,
};
use tokio::net::UdpSocket;

/// 两个端点必须相同的配对 tag。
const TAG: Tag = [0x5Au8; 16];
/// 中继共享密钥（与 Config.key 一致，32 字节）。
const KEY: [u8; 32] = [0x42u8; 32];
const FRAMES: u64 = 10;
const FRAME_PAYLOAD: usize = 1024;
const HANDSHAKE_PASSWORD: &str = "relay-password";

fn relay_key() -> RelayKey {
    RelayKey::from_bytes(&KEY).expect("32 字节 relay_key 合法")
}

async fn start_relay() -> RelayHandle {
    let cfg = Config {
        // 绑定 0 端口：由内核选一个空闲 loopback 端口，避免测试间抢端口。
        bind: "127.0.0.1:0".parse().expect("合法地址"),
        tcp_bind: None,
        admin_addr: None,
        key: relay_key(),
        log: LogLevel::Off,
        log_json: false,
        no_log_ips: true,
        limits: Limits {
            max_sessions: 8,
            // 不限速：QUIC 握手 + 10 帧突发远小于默认 200Mbps，但令牌桶的
            // burst 下限（64KiB）会误伤短突发，测试里直接关掉限速。
            per_tag_bytes_per_s: None,
            global_bytes_per_s: None,
            tag_ttl: Duration::from_secs(3600),
            idle_timeout: Duration::from_secs(60),
            bind_window_s: 300,
            nonce_capacity: 8192,
            rebind_guard: Duration::from_millis(0),
        },
    };
    relay::start(Arc::new(cfg)).await.expect("启动中继失败")
}

fn relay_cfg(relay_addr: SocketAddr, role: Role) -> RelayClientConfig {
    RelayClientConfig {
        relay_addr,
        tag: TAG,
        role,
        key: KEY,
        nonce: 0x0102_0304_0506_0708,
    }
}

/// 测试缩短 bind 重发时间片；保活拉长到 60s，避免测试期间产生额外包。
fn fast_opts() -> RelaySocketOptions {
    RelaySocketOptions {
        bind_timeout: Duration::from_secs(3),
        retry_interval: Duration::from_millis(100),
        keepalive_interval: Duration::from_secs(60),
    }
}

/// 用 RelaySocket 的 socket 副本包装成 tokio socket（未交给 quinn，读副本安全）。
fn wrap(socket: &dfy_relay::RelaySocket, who: &str) -> UdpSocket {
    let std = socket
        .try_clone_socket()
        .unwrap_or_else(|e| panic!("复制 {who} relay socket 失败: {e}"));
    UdpSocket::from_std(std).unwrap_or_else(|e| panic!("包装 {who} relay socket 失败: {e}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_socket_binds_forwards_and_keepalive_is_idempotent() {
    let relay = start_relay().await;
    let relay_addr = relay.udp_local;

    // ---- 两个角色分别 bind（中继在这里完成配对）----
    let agent = bind_relay_socket_with(&relay_cfg(relay_addr, Role::Agent), &fast_opts())
        .await
        .expect("Agent bind 失败");
    let client = bind_relay_socket_with(&relay_cfg(relay_addr, Role::Client), &fast_opts())
        .await
        .expect("Client bind 失败");

    assert_eq!(agent.peer_addr(), relay_addr);
    assert_eq!(client.peer_addr(), relay_addr);
    assert_eq!(agent.role(), Role::Agent);
    assert_eq!(client.role(), Role::Client);
    assert_eq!(*agent.tag(), TAG);
    assert!(agent.expires_in_s() > 0, "BindOk 应带回 tag 剩余 TTL");
    assert_ne!(
        agent.local_addr().expect("agent local_addr"),
        client.local_addr().expect("client local_addr")
    );

    // ---- 双向 UDP 转发（数据面在 bind 之后原样转发）----
    let agent_sock = wrap(&agent, "agent");
    let client_sock = wrap(&client, "client");
    let mut buf = vec![0u8; 65535];

    let payload = vec![0xABu8; 1024];
    agent_sock
        .send_to(&payload, relay_addr)
        .await
        .expect("agent 发送失败");
    let (n, from) = tokio::time::timeout(Duration::from_secs(2), client_sock.recv_from(&mut buf))
        .await
        .expect("client 收包超时")
        .expect("client 收包失败");
    assert_eq!(from, relay_addr, "经中继转发后源地址应是中继地址");
    assert_eq!(&buf[..n], payload.as_slice());

    let reply = vec![0xCDu8; 512];
    client_sock
        .send_to(&reply, relay_addr)
        .await
        .expect("client 发送失败");
    let (n, from) = tokio::time::timeout(Duration::from_secs(2), agent_sock.recv_from(&mut buf))
        .await
        .expect("agent 收包超时")
        .expect("agent 收包失败");
    assert_eq!(from, relay_addr);
    assert_eq!(&buf[..n], reply.as_slice());

    // ---- 保活：同地址重复 bind 幂等（回 BindOk，不计 rebind、不换端点）----
    let before_out = relay.metrics.bytes_out();
    let before_rebinds = relay.metrics.rebinds();
    agent
        .send_bind_request()
        .expect("发送保活 BindRequest 失败");
    let (n, from) = tokio::time::timeout(Duration::from_secs(2), agent_sock.recv_from(&mut buf))
        .await
        .expect("保活 BindOk 超时")
        .expect("保活收包失败");
    assert_eq!(from, relay_addr);
    assert_eq!(proto::classify(&buf[..n]), proto::PacketClass::BindOk);
    assert!(
        relay.metrics.bytes_out() >= before_out + proto::BIND_OK_LEN as u64,
        "保活应得到 BindOk 回包"
    );
    assert_eq!(
        relay.metrics.rebinds(),
        before_rebinds,
        "同地址重复 bind 不应计 rebind"
    );

    // ---- 中继统计 ----
    assert_eq!(relay.metrics.sessions(), 1, "同一 tag 只应有一个会话");
    assert!(relay.metrics.bytes_in() > 0);
    assert!(relay.metrics.bytes_out() > 0);
    for reason in [
        DropReason::Unpaired,
        DropReason::UnknownSource,
        DropReason::Auth,
        DropReason::Replay,
        DropReason::BadPacket,
    ] {
        assert_eq!(
            relay.metrics.dropped(reason),
            0,
            "不应出现 {} 丢弃",
            reason.label()
        );
    }

    relay.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quic_over_relay_handshake_video_and_ping() {
    dfy_transport::tls::install_crypto_provider();

    let relay = start_relay().await;
    let relay_addr = relay.udp_local;

    // ---- 两端先完成中继 bind ----
    let agent_relay = bind_relay_socket_with(&relay_cfg(relay_addr, Role::Agent), &fast_opts())
        .await
        .expect("Agent bind 失败");
    let client_relay = bind_relay_socket_with(&relay_cfg(relay_addr, Role::Client), &fast_opts())
        .await
        .expect("Client bind 失败");

    // ---- Agent QUIC endpoint：server 配置 + relay socket 副本 ----
    let identity = TlsIdentity::generate("relay-agent").expect("生成 Agent 身份");
    let fingerprint = identity.fingerprint;
    let agent_endpoint = quinn::Endpoint::new(
        quinn::EndpointConfig::default(),
        Some(server_config(&identity).expect("构造 Agent server config")),
        agent_relay
            .try_clone_socket()
            .expect("复制 agent relay socket 失败"),
        Arc::new(quinn::TokioRuntime),
    )
    .expect("构造 Agent QUIC endpoint 失败");

    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
    let agent_task = tokio::spawn(async move {
        let incoming = agent_endpoint
            .accept()
            .await
            .expect("端点已关闭，无法 accept");
        let conn = tokio::time::timeout(Duration::from_secs(10), incoming)
            .await
            .expect("等待 QUIC 连接超时")
            .expect("QUIC 连接建立失败");
        let (welcome, channel) =
            server_handshake(&conn, &ServerAuth::new(HANDSHAKE_PASSWORD, "relay-agent"))
                .await
                .expect("Agent 握手失败");
        let mut session = AgentSession::new(conn.clone(), welcome, channel);

        // 10 帧 1KB 视频 DATAGRAM（首帧关键帧）；1KB 在默认 MTU 下应每帧 1 包。
        let payload = vec![0xCDu8; FRAME_PAYLOAD];
        let mut packets = 0usize;
        for i in 0..FRAMES as u32 {
            let stats = send_video_frame(&conn, i, i as u16, i * 10_000, &payload, i == 0)
                .expect("发送视频 DATAGRAM 失败");
            packets += stats.packets;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // 等 Client 的 Ping，回 Pong（与真实 Agent 心跳路径一致）。
        let env = tokio::time::timeout(Duration::from_secs(5), session.recv_ctrl())
            .await
            .expect("等待 Client Ping 超时")
            .expect("recv_ctrl 失败")
            .expect("control 流提前关闭");
        let reply = session.heartbeat.on_frame(&env).expect("Ping 应产生 Pong");
        session.send_ctrl(&reply).await.expect("发送 Pong 失败");

        // 等 Client 收完帧再关连接（避免 DATAGRAM 因连接关闭被截断）。
        let _ = tokio::time::timeout(Duration::from_secs(5), done_rx).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        agent_endpoint.close(0u32.into(), b"done");
        packets
    });

    // ---- Client QUIC endpoint：client 配置 + relay socket 副本 ----
    let tls = client_tls(Some(fingerprint)).expect("构造 Client TLS 失败");
    let mut client_endpoint = quinn::Endpoint::new(
        quinn::EndpointConfig::default(),
        None,
        client_relay
            .try_clone_socket()
            .expect("复制 client relay socket 失败"),
        Arc::new(quinn::TokioRuntime),
    )
    .expect("构造 Client QUIC endpoint 失败");
    client_endpoint.set_default_client_config(tls.config.clone());

    // 目标地址是**中继地址**，而不是 Agent 的真实地址（Agent 在 NAT 后面）。
    let connecting = client_endpoint
        .connect(relay_addr, "localhost")
        .expect("发起 QUIC connect 失败");
    let conn = tokio::time::timeout(Duration::from_secs(10), connecting)
        .await
        .expect("QUIC 握手超时（中继路径未打通）")
        .expect("QUIC 握手失败");
    let (welcome, channel) =
        client_handshake(&conn, HANDSHAKE_PASSWORD, "relay-client", vec![9, 9, 9])
            .await
            .expect("Client 握手失败");
    assert_eq!(welcome.agent_name, "relay-agent");
    assert_eq!(
        tls.observed_hex().as_deref(),
        Some(hex::encode(fingerprint).as_str()),
        "TOFU 应观察到 Agent 的 SPKI 指纹"
    );

    let mut session = ClientSession::new(conn.clone(), welcome, channel);
    let rtt_us = tokio::time::timeout(Duration::from_secs(5), session.ping_once())
        .await
        .expect("Ping/Pong 超时")
        .expect("Ping/Pong 失败");
    assert!(rtt_us > 0, "RTT 应为正数，实际 {rtt_us}us");

    // 收满 10 帧视频 DATAGRAM。
    let mut receiver = VideoReceiver::new();
    while receiver.frames() < FRAMES as usize {
        let datagram = tokio::time::timeout(Duration::from_secs(5), session.conn.read_datagram())
            .await
            .expect("等待视频 DATAGRAM 超时")
            .expect("读取 DATAGRAM 失败");
        receiver
            .ingest(datagram.as_ref())
            .expect("视频 DATAGRAM 解析失败");
    }
    let _ = done_tx.send(());

    let agent_packets = tokio::time::timeout(Duration::from_secs(10), agent_task)
        .await
        .expect("Agent 任务超时")
        .expect("Agent 任务 panic");

    // ---- 统计断言 ----
    assert_eq!(receiver.frames(), FRAMES as usize, "应重组出全部 10 帧");
    assert!(
        receiver.packets() >= FRAMES,
        "至少每帧一个 DATAGRAM，实际 {} 包",
        receiver.packets()
    );
    assert!(
        agent_packets >= FRAMES as usize,
        "Agent 发送包数 {agent_packets}"
    );
    assert_eq!(receiver.malformed(), 0, "不应出现 DATAGRAM 解析失败");
    assert_eq!(receiver.out_of_order(), 0, "loopback 不应乱序");
    assert!(receiver.key_packets() >= 1, "首帧应是关键帧");

    assert!(relay.metrics.bytes_in() > 0 && relay.metrics.bytes_out() > 0);
    assert_eq!(
        relay.metrics.dropped(DropReason::BadPacket),
        0,
        "中继不应把 QUIC 包误判成坏控制包"
    );
    assert_eq!(relay.metrics.dropped(DropReason::Unpaired), 0);
    assert_eq!(relay.metrics.dropped(DropReason::UnknownSource), 0);
    assert_eq!(relay.metrics.sessions(), 1, "同一 tag 只应有一个会话");

    relay.stop().await;
}
