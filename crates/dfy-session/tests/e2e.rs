// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 端到端（loopback）测试：真实 QUIC 连接 + 真实握手 + 假采集/编码/解码，
//! 验证「采集线程 → Agent 发送 → 网络 → Client 重组 → 解码 → 回调」整条流水线。

use std::sync::Arc;
use std::time::Duration;

use dfy_session::{
    run_client, serve_agent, start_capture_loop, AgentConfig, CaptureLoopConfig, ClientConfig,
    FakeCapturer, FakeDecoder, FakeEncoder, IgnoreHandler,
};
use dfy_transport::{
    bind_agent_random, client_handshake, client_tls, connect, server_handshake,
    tls::install_crypto_provider, ServerAuth,
};
use tokio::sync::Mutex;

const FRAMES: u64 = 32;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loopback_session_delivers_all_frames() {
    install_crypto_provider();

    let (endpoint, identity, addr) = bind_agent_random().expect("绑定 Agent 端点");
    let fingerprint = identity.fingerprint;

    // ---- Agent 侧：接受连接、握手、启动采集线程、串流 ----
    let agent_task = tokio::spawn(async move {
        let incoming = endpoint.accept().await.expect("端点已关闭，无法 accept");
        let conn = incoming.await.expect("QUIC 连接建立失败");
        let (welcome, chan) =
            server_handshake(&conn, &ServerAuth::new("test-password", "e2e-agent"))
                .await
                .expect("Agent 握手失败");

        let mut capture = start_capture_loop(
            CaptureLoopConfig {
                capture_timeout_ms: 1,
                max_fps: 60,
                queue_capacity: 32,
                max_consecutive_errors: 3,
            },
            || Ok((FakeCapturer::new(64, 48, FRAMES * 4), FakeEncoder::new())),
        )
        .expect("启动采集线程失败");

        let cfg = AgentConfig {
            max_frames: Some(FRAMES),
            idle_timeout: Duration::from_secs(30),
            drain_delay: Duration::from_millis(200),
            stay_alive: false,
        };
        let stats = serve_agent(conn, welcome, chan, &mut capture, IgnoreHandler, cfg)
            .await
            .expect("Agent 会话失败");
        endpoint.close(0u32.into(), b"done");
        stats
    });

    // ---- Client 侧：拨号、握手、接收 ----
    let tls = client_tls(Some(fingerprint)).expect("构造客户端 TLS");
    let (_client_ep, conn) = connect(addr, tls).await.expect("连接 Agent 失败");
    let (welcome, chan) = client_handshake(&conn, "test-password", "e2e-client", vec![1, 2, 3])
        .await
        .expect("Client 握手失败");
    assert_eq!(welcome.agent_name, "e2e-agent");

    let delivered = Arc::new(Mutex::new(Vec::<u32>::new()));
    let sink = Arc::clone(&delivered);
    let stats = run_client(
        conn,
        welcome,
        chan,
        FakeDecoder::new(),
        move |frame| {
            // 回调是同步的；用 try_lock 避免在回调里 await（多线程 runtime 下也安全）
            if let Ok(mut v) = sink.try_lock() {
                v.push(frame.pts_us as u32);
            }
        },
        ClientConfig::default()
            .with_expect_frames(FRAMES)
            .with_ping_interval(Duration::from_millis(300)),
    )
    .await
    .expect("Client 会话失败");

    let agent_stats = tokio::time::timeout(Duration::from_secs(10), agent_task)
        .await
        .expect("Agent 任务超时")
        .expect("Agent 任务 panic");

    assert!(
        stats.frames_delivered >= FRAMES - 1,
        "Client 交付帧数 {} 少于预期 {}（QUIC DATAGRAM 本身不可靠，允许偶发丢 1 帧）",
        stats.frames_delivered,
        FRAMES
    );
    assert_eq!(agent_stats.frames_sent, FRAMES, "Agent 发送帧数");
    assert!(stats.packets_recv >= FRAMES, "至少每帧一个 DATAGRAM");
    assert_eq!(stats.malformed, 0, "不应出现解析失败");
    assert!(stats.keyframes >= 1, "至少一个关键帧");
    assert!(agent_stats.keyframes_sent >= 1, "Agent 至少发过关键帧");
    assert!(
        stats.rtt_ms.is_some() || agent_stats.rtt_ms.is_some(),
        "心跳应测到 RTT"
    );
    assert!(
        stats.frames_delivered <= agent_stats.frames_sent,
        "客户端不可能收到比发送更多的帧"
    );
    assert!(
        matches!(stats.end_reason, "expect_frames" | "agent_bye"),
        "结束原因应为收满帧或对端 Bye，实际 {}",
        stats.end_reason
    );
}

/// `run_client` 的配置构造器（避免上面测试里方法名拼写错误时静默通过）。
#[test]
fn client_config_has_ping_interval_builder() {
    let cfg = ClientConfig::default().with_ping_interval(Duration::from_millis(250));
    assert_eq!(cfg.ping_interval, Duration::from_millis(250));
}
