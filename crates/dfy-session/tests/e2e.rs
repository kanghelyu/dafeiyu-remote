// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 端到端（loopback）测试：真实 QUIC 连接 + 真实握手 + 假采集/编码/解码，
//! 验证「采集线程 → Agent 发送 → 网络 → Client 重组 → 解码 → 回调」整条流水线。

use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex as StdMutex,
};
use std::time::Duration;

use anyhow::{Context, Result};
use dfy_proto::{
    decode_video, envelope, Envelope, SessionResume, StreamFeedback, VideoHeader, PERM_CONTROL,
    PERM_VIEW, SESSION_RESUME_COMMIT, SESSION_RESUME_PREPARE,
};
use dfy_session::{
    run_client, serve_agent, serve_agent_shared, start_capture_loop,
    start_capture_loop_with_encoder_factory, AgentConfig, CaptureLoop, CaptureLoopConfig,
    ClientConfig, EncodedFrame, Encoder, FakeCapturer, FakeDecoder, FakeEncoder, IgnoreHandler,
    MigrationContext, RawFrame, RuntimeSettings, SessionCoordinator, SessionLease,
    StreamAdaptationConfig,
};
use dfy_transport::{
    bind_agent_random, client_handshake, client_handshake_context,
    client_handshake_context_for_session, client_resume_handshake, client_tls, connect,
    server_handshake, server_handshake_context, tls::install_crypto_provider, ServerAuth,
};
use ed25519_dalek::SigningKey;
use tokio::sync::{oneshot, Mutex};

const FRAMES: u64 = 64;

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
            ..AgentConfig::default()
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
    assert!(
        stats.feedback_windows_sent >= 1,
        "Client 应自动发送质量反馈"
    );
    assert!(agent_stats.feedback_windows >= 1, "Agent 应收到质量反馈");
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

/// 新路径先完整认证，再签名 PREPARE/COMMIT；旧连接停发，帧号跨连接连续。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn authenticated_migration_fences_connections_and_preserves_frame_ids() -> Result<()> {
    install_crypto_provider();
    const PASSWORD: &str = "test-password";
    const TIMEOUT: Duration = Duration::from_secs(20);

    let mut auth = ServerAuth::new(PASSWORD, "e2e-agent");
    auth.permission_bitmap = PERM_VIEW | PERM_CONTROL;
    let coordinator = SessionCoordinator::default();
    let key = SigningKey::from_bytes(&[42; 32]);
    let pubkey = key.verifying_key().to_bytes().to_vec();
    let (endpoint, identity, addr) = bind_agent_random()?;
    let fingerprint = identity.fingerprint;

    let tls1 = client_tls(Some(fingerprint))?;
    let (client1, server_conn1) = tokio::time::timeout(TIMEOUT, async {
        tokio::join!(connect(addr, tls1), async {
            endpoint
                .accept()
                .await
                .expect("endpoint closed")
                .await
                .expect("first accept failed")
        },)
    })
    .await
    .context("first QUIC connection timeout")?;
    let (_client_ep1, client_conn1) = client1?;
    let (server1, client1) = tokio::time::timeout(TIMEOUT, async {
        tokio::join!(
            server_handshake_context(&server_conn1, &auth),
            client_handshake_context(&client_conn1, PASSWORD, "e2e-client", pubkey.clone()),
        )
    })
    .await
    .context("first authenticated handshake timeout")?;
    let server1 = server1?;
    let client1 = client1?;
    let mut previous = client1.welcome.clone();
    assert_eq!(previous, server1.welcome);
    previous.permission_bitmap = PERM_VIEW;
    let context1 = MigrationContext {
        auth: auth.clone(),
        hello: server1.hello.clone(),
        exporter: server1.exporter,
        coordinator: coordinator.clone(),
    };
    let mut lease1 = SessionLease::initial(&context1, &server1.welcome)?;
    assert!(lease1.quality_control_allowed());

    for expected in 0..4u32 {
        send_and_receive(&lease1, &server_conn1, &client_conn1, expected).await?;
    }

    let tls2 = client_tls(Some(fingerprint))?;
    let (client2, server_conn2) = tokio::time::timeout(TIMEOUT, async {
        tokio::join!(connect(addr, tls2), async {
            endpoint
                .accept()
                .await
                .expect("endpoint closed")
                .await
                .expect("second accept failed")
        },)
    })
    .await
    .context("second QUIC connection timeout")?;
    let (_client_ep2, client_conn2) = client2?;
    let observed = lease1.clone();
    let changed = observed.changed();
    tokio::pin!(changed);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut changed)
            .await
            .is_err(),
        "old lease woke before migration"
    );
    let (server_result, client_result) = tokio::time::timeout(TIMEOUT, async {
        tokio::join!(
            async {
                let server = server_handshake_context(&server_conn2, &auth).await?;
                let temporary_welcome = server.welcome.clone();
                let context = MigrationContext {
                    auth: auth.clone(),
                    hello: server.hello.clone(),
                    exporter: server.exporter,
                    coordinator: coordinator.clone(),
                };
                let mut lease = SessionLease::pending(&context, &temporary_welcome);
                let (mut tx, mut rx) = server.chan.split();
                let undeclared = signed_prepare(&key, &server.hello, &server.exporter, &previous)?;
                let mut undeclared_context = context.clone();
                undeclared_context.hello.resume_session_id.clear();
                let error = lease
                    .handle_resume(&undeclared_context, &mut tx, &undeclared)
                    .await
                    .unwrap_err()
                    .to_string();
                assert!(error.contains("not declared"));
                let mut commit = None;
                for expected in [SESSION_RESUME_PREPARE, SESSION_RESUME_COMMIT] {
                    let env = rx.recv_ctrl().await?.context("missing resume request")?;
                    let request = match env.body {
                        Some(envelope::Body::SessionResume(request)) => request,
                        _ => anyhow::bail!("expected SessionResume"),
                    };
                    assert_eq!(request.phase, expected);
                    if expected == SESSION_RESUME_COMMIT {
                        commit = Some(request.clone());
                    }
                    lease.handle_resume(&context, &mut tx, &request).await?;
                }
                anyhow::Ok((temporary_welcome, context, lease, tx, commit))
            },
            client_resume_handshake(&client_conn2, PASSWORD, "e2e-client", &key, &previous, 3, 3),
        )
    })
    .await
    .context("resume timeout")?;
    let (temporary_welcome, context2, mut lease2, mut tx2, commit) = server_result?;
    let (resumed, client_chan2) = client_result?;
    let (_client_tx2, mut client_rx2) = client_chan2.split();
    assert_eq!(resumed.session_id, previous.session_id);
    assert_eq!(resumed.session_epoch, previous.session_epoch + 1);
    assert_eq!(resumed.permission_bitmap, PERM_VIEW);
    assert_eq!(lease2.permissions(), PERM_VIEW);
    assert!(!lease2.quality_control_allowed());
    assert_ne!(resumed.resume_token, previous.resume_token);
    tokio::time::timeout(Duration::from_secs(1), &mut changed).await?;
    assert!(lease1.was_superseded());
    assert!(lease1
        .send_frame(&server_conn1, 99, b"old", true)?
        .is_none());

    let commit = commit.context("missing COMMIT")?;
    let error = lease2
        .handle_resume_with_commit(&context2, &mut tx2, &commit, || {
            anyhow::bail!("capture stopped before migration commit")
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("capture stopped"));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), client_rx2.recv_ctrl())
            .await
            .is_err(),
        "failed commit hook must not send SessionResumeOk"
    );
    lease2.handle_resume(&context2, &mut tx2, &commit).await?;
    let ack = tokio::time::timeout(Duration::from_secs(1), client_rx2.recv_ctrl())
        .await
        .context("timed out waiting for successful COMMIT ack")??
        .context("missing successful COMMIT ack")?;
    assert!(matches!(ack.body, Some(envelope::Body::SessionResumeOk(_))));
    let mut replay = SessionLease::pending(&context2, &temporary_welcome);
    let error = replay
        .handle_resume(&context2, &mut tx2, &commit)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("another connection"));
    replay.finish(&context2, false);
    let mut legacy = commit;
    legacy.phase = 0;
    let error = lease2
        .handle_resume(&context2, &mut tx2, &legacy)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("legacy"));

    for expected in 4..6u32 {
        send_and_receive(&lease2, &server_conn2, &client_conn2, expected).await?;
    }

    let revoked = signed_prepare(
        &key,
        &context2.hello,
        &context2.exporter,
        &temporary_welcome,
    )?;
    lease2.finish(&context2, true);
    lease1.finish(&context1, true);
    let error = auth
        .resume_registry
        .prepare(&context2.hello, &context2.exporter, &revoked)
        .err()
        .expect("revoked grant unexpectedly accepted")
        .to_string();
    assert!(
        error.contains("恢复令牌未知"),
        "unused welcome grant was not revoked: {error}"
    );
    assert!(!coordinator.contains(&previous.session_id));
    endpoint.close(0u32.into(), b"done");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_first_commit_preserves_migration_grant_and_owner() -> Result<()> {
    install_crypto_provider();
    const PASSWORD: &str = "test-password";
    const TIMEOUT: Duration = Duration::from_secs(20);
    let mut auth = ServerAuth::new(PASSWORD, "first-commit-agent");
    auth.permission_bitmap = PERM_VIEW | PERM_CONTROL;
    let coordinator = SessionCoordinator::default();
    let key = SigningKey::from_bytes(&[44; 32]);
    let pubkey = key.verifying_key().to_bytes().to_vec();
    let (endpoint, identity, addr) = bind_agent_random()?;

    let tls1 = client_tls(Some(identity.fingerprint))?;
    let (client1, server_conn1) = tokio::time::timeout(TIMEOUT, async {
        tokio::join!(connect(addr, tls1), async {
            endpoint
                .accept()
                .await
                .expect("endpoint closed")
                .await
                .expect("first accept failed")
        },)
    })
    .await
    .context("first QUIC connection timeout")?;
    let (_client_ep1, client_conn1) = client1?;
    let (server1, client1) = tokio::time::timeout(TIMEOUT, async {
        tokio::join!(
            server_handshake_context(&server_conn1, &auth),
            client_handshake_context(
                &client_conn1,
                PASSWORD,
                "first-commit-client",
                pubkey.clone()
            ),
        )
    })
    .await
    .context("first authenticated handshake timeout")?;
    let server1 = server1?;
    let client1 = client1?;
    let mut previous = client1.welcome.clone();
    previous.permission_bitmap = PERM_VIEW;
    let context1 = MigrationContext {
        auth: auth.clone(),
        hello: server1.hello.clone(),
        exporter: server1.exporter,
        coordinator: coordinator.clone(),
    };
    let mut lease1 = SessionLease::initial(&context1, &server1.welcome)?;
    send_and_receive(&lease1, &server_conn1, &client_conn1, 0).await?;

    let tls2 = client_tls(Some(identity.fingerprint))?;
    let (client2, server_conn2) = tokio::time::timeout(TIMEOUT, async {
        tokio::join!(connect(addr, tls2), async {
            endpoint
                .accept()
                .await
                .expect("endpoint closed")
                .await
                .expect("second accept failed")
        },)
    })
    .await
    .context("second QUIC connection timeout")?;
    let (_client_ep2, client_conn2) = client2?;
    let (server2, client2) = tokio::time::timeout(TIMEOUT, async {
        tokio::join!(
            server_handshake_context(&server_conn2, &auth),
            client_handshake_context_for_session(
                &client_conn2,
                PASSWORD,
                "first-commit-client",
                pubkey.clone(),
                previous.session_id.clone(),
            ),
        )
    })
    .await
    .context("resume authenticated handshake timeout")?;
    let server2 = server2?;
    let client2 = client2?;
    let context2 = MigrationContext {
        auth: auth.clone(),
        hello: server2.hello.clone(),
        exporter: server2.exporter,
        coordinator: coordinator.clone(),
    };
    let mut lease2 = SessionLease::pending(&context2, &server2.welcome);
    let (mut server_tx, mut server_rx) = server2.chan.split();
    let (mut client_tx, mut client_rx) = client2.chan.split();

    let mut prepare = signed_prepare(&key, &context2.hello, &context2.exporter, &previous)?;
    prepare.last_frame_id = 1;
    prepare.signature =
        dfy_transport::resume::sign_request(&key, &context2.hello, &context2.exporter, &prepare)?;
    client_tx
        .send_ctrl(&Envelope {
            body: Some(envelope::Body::SessionResume(prepare.clone())),
        })
        .await?;
    let request = tokio::time::timeout(TIMEOUT, server_rx.recv_ctrl())
        .await??
        .context("missing PREPARE")?;
    let Some(envelope::Body::SessionResume(request)) = request.body else {
        anyhow::bail!("expected PREPARE");
    };
    assert_eq!(request.phase, SESSION_RESUME_PREPARE);
    lease2
        .handle_resume(&context2, &mut server_tx, &request)
        .await?;
    let prepared = recv_resume_ack(&mut client_rx).await?;
    assert_eq!(prepared.resume_from_frame_id, 1);

    let mut commit = prepare;
    commit.phase = SESSION_RESUME_COMMIT;
    commit.server_nonce = prepared.server_nonce;
    commit.signature =
        dfy_transport::resume::sign_request(&key, &context2.hello, &context2.exporter, &commit)?;
    client_tx
        .send_ctrl(&Envelope {
            body: Some(envelope::Body::SessionResume(commit.clone())),
        })
        .await?;
    let request = tokio::time::timeout(TIMEOUT, server_rx.recv_ctrl())
        .await??
        .context("missing COMMIT")?;
    let Some(envelope::Body::SessionResume(request)) = request.body else {
        anyhow::bail!("expected COMMIT");
    };
    assert_eq!(request.phase, SESSION_RESUME_COMMIT);

    let error = lease2
        .handle_resume_with_commit(&context2, &mut server_tx, &request, || {
            anyhow::bail!("capture stopped before first migration commit")
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("capture stopped"));
    assert!(!lease1.was_superseded());
    assert!(
        tokio::time::timeout(Duration::from_millis(100), client_rx.recv_ctrl())
            .await
            .is_err(),
        "failed first COMMIT must not send SessionResumeOk"
    );

    lease2
        .handle_resume_with_commit(&context2, &mut server_tx, &request, || Ok(()))
        .await?;
    let committed = recv_resume_ack(&mut client_rx).await?;
    assert_eq!(committed.session_epoch, previous.session_epoch + 1);
    assert_eq!(committed.resume_from_frame_id, 1);
    assert!(lease1.was_superseded());
    assert_ne!(committed.resume_token, previous.resume_token);

    client_conn2.close(0u32.into(), b"inject ack failure");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let error = lease2
        .handle_resume_with_commit(&context2, &mut server_tx, &request, || Ok(()))
        .await
        .unwrap_err();
    assert!(error
        .downcast_ref::<dfy_session::migration::MigrationAckFailed>()
        .is_some());
    assert!(lease1.was_superseded());
    assert!(lease2.is_active());

    let mut committed_welcome = previous.clone();
    committed_welcome.session_epoch = committed.session_epoch;
    committed_welcome.resume_token = committed.resume_token.clone();
    lease2.finish(&context2, false);
    lease1.finish(&context1, false);

    let tls3 = client_tls(Some(identity.fingerprint))?;
    let (client3, server_conn3) = tokio::time::timeout(TIMEOUT, async {
        tokio::join!(connect(addr, tls3), async {
            endpoint
                .accept()
                .await
                .expect("endpoint closed")
                .await
                .expect("third accept failed")
        },)
    })
    .await
    .context("third QUIC connection timeout")?;
    let (_client_ep3, client_conn3) = client3?;
    let (server3, client3) = tokio::time::timeout(TIMEOUT, async {
        tokio::join!(
            server_handshake_context(&server_conn3, &auth),
            client_handshake_context_for_session(
                &client_conn3,
                PASSWORD,
                "first-commit-client",
                pubkey.clone(),
                committed_welcome.session_id.clone(),
            ),
        )
    })
    .await
    .context("reclaim authenticated handshake timeout")?;
    let server3 = server3?;
    let client3 = client3?;
    let context3 = MigrationContext {
        auth: auth.clone(),
        hello: server3.hello.clone(),
        exporter: server3.exporter,
        coordinator: coordinator.clone(),
    };
    let mut lease3 = SessionLease::pending(&context3, &server3.welcome);
    let (mut server_tx3, mut server_rx3) = server3.chan.split();
    let (mut client_tx3, mut client_rx3) = client3.chan.split();
    let mut commit3 = commit.clone();
    commit3.signature =
        dfy_transport::resume::sign_request(&key, &context3.hello, &context3.exporter, &commit3)?;
    client_tx3
        .send_ctrl(&Envelope {
            body: Some(envelope::Body::SessionResume(commit3.clone())),
        })
        .await?;
    let request3 = tokio::time::timeout(TIMEOUT, server_rx3.recv_ctrl())
        .await??
        .context("missing reclaim COMMIT")?;
    let Some(envelope::Body::SessionResume(request3)) = request3.body else {
        anyhow::bail!("expected reclaim COMMIT");
    };
    lease3
        .handle_resume_with_commit(&context3, &mut server_tx3, &request3, || Ok(()))
        .await?;
    let reclaimed = recv_resume_ack(&mut client_rx3).await?;
    assert_eq!(reclaimed.session_epoch, committed_welcome.session_epoch);
    assert_eq!(reclaimed.resume_token, committed_welcome.resume_token);
    assert_eq!(reclaimed.resume_from_frame_id, 1);
    assert!(lease3.is_active());
    send_and_receive(&lease3, &server_conn3, &client_conn3, 1).await?;

    lease3.finish(&context3, true);
    endpoint.close(0u32.into(), b"done");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_agent_migration_stops_old_stream_and_requests_keyframe() -> Result<()> {
    install_crypto_provider();
    const PASSWORD: &str = "test-password";
    const TIMEOUT: Duration = Duration::from_secs(20);
    let auth = ServerAuth::new(PASSWORD, "e2e-agent");
    let coordinator = SessionCoordinator::default();
    let key = SigningKey::from_bytes(&[43; 32]);
    let pubkey = key.verifying_key().to_bytes().to_vec();
    let (endpoint, identity, addr) = bind_agent_random()?;
    let fingerprint = identity.fingerprint;
    let (connections_tx, mut connections_rx) = tokio::sync::mpsc::channel(2);
    let acceptor = tokio::spawn(async move {
        for _ in 0..2 {
            let incoming = endpoint.accept().await.expect("endpoint closed");
            let conn = incoming.await.expect("QUIC accept failed");
            connections_tx
                .send(conn)
                .await
                .expect("connection queue closed");
        }
    });
    let keyframe_requested = Arc::new(AtomicBool::new(false));
    let (capture_tx, capture_rx) = tokio::sync::oneshot::channel();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let owner_keyframe_requested = Arc::clone(&keyframe_requested);
    let capture_owner = tokio::spawn(async move {
        let mut capture = start_capture_loop(
            CaptureLoopConfig {
                capture_timeout_ms: 1,
                max_fps: 60,
                queue_capacity: 32,
                max_consecutive_errors: 3,
            },
            || {
                Ok((
                    FakeCapturer::new(64, 48, u64::MAX),
                    NonKeyframeEncoder::new(owner_keyframe_requested),
                ))
            },
        )?;
        let handle = capture.shared_handle();
        capture_tx
            .send(handle)
            .map_err(|_| anyhow::anyhow!("capture receiver dropped"))?;
        let _ = stop_rx.await;
        capture.stop();
        capture.join();
        anyhow::Ok(())
    });
    let capture = capture_rx.await?;

    let (_client_ep1, client_conn1) = connect(addr, client_tls(Some(fingerprint))?).await?;
    let server_conn1 = connections_rx
        .recv()
        .await
        .context("missing first connection")?;
    let (client1, server1) = tokio::time::timeout(TIMEOUT, async {
        tokio::join!(
            client_handshake_context(&client_conn1, PASSWORD, "e2e-client", pubkey.clone()),
            server_handshake_context(&server_conn1, &auth),
        )
    })
    .await
    .context("first authenticated handshake timeout")?;
    let client1 = client1?;
    let server1 = server1?;
    assert_eq!(client1.welcome, server1.welcome);
    let (mut client_tx1, mut client_rx1) = client1.chan.split();
    let first_client = tokio::spawn(async move {
        while let Ok(Some(env)) = client_rx1.recv_ctrl().await {
            if let Some(dfy_proto::envelope::Body::Ping(ping)) = env.body {
                if client_tx1
                    .send_ctrl(&dfy_proto::Envelope::pong(ping.ts_us))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    });
    let first_lease_context = MigrationContext {
        auth: auth.clone(),
        hello: server1.hello.clone(),
        exporter: server1.exporter,
        coordinator: coordinator.clone(),
    };
    let first_capture = capture.clone();
    let first_agent_conn = server_conn1;
    let first_agent_coordinator = coordinator.clone();
    let mut first_agent = tokio::spawn(async move {
        serve_agent_shared(
            first_agent_conn,
            server1.welcome,
            server1.chan,
            &first_capture,
            IgnoreHandler,
            AgentConfig {
                idle_timeout: Duration::from_secs(30),
                drain_delay: Duration::from_millis(50),
                stay_alive: true,
                migration: Some(first_lease_context),
                coordinator: first_agent_coordinator,
                ..AgentConfig::default()
            },
        )
        .await
    });

    let first_datagram = tokio::select! {
        datagram = tokio::time::timeout(Duration::from_secs(5), client_conn1.read_datagram()) => datagram??,
        result = &mut first_agent => {
            anyhow::bail!("first agent ended before video: {result:?}")
        }
    };
    let (first_header, _) = decode_video(&first_datagram)?;

    let (client2, client_conn2) = connect(addr, client_tls(Some(fingerprint))?).await?;
    let _client_ep2 = client2;
    let server_conn2 = connections_rx
        .recv()
        .await
        .context("missing second connection")?;
    let second_agent_conn = server_conn2.clone();
    let second_agent = tokio::spawn(async move {
        let context = server_handshake_context(&second_agent_conn, &auth).await?;
        let migration = MigrationContext {
            auth: auth.clone(),
            hello: context.hello.clone(),
            exporter: context.exporter,
            coordinator: coordinator.clone(),
        };
        serve_agent_shared(
            second_agent_conn.clone(),
            context.welcome,
            context.chan,
            &capture,
            IgnoreHandler,
            AgentConfig {
                idle_timeout: Duration::from_secs(30),
                drain_delay: Duration::from_millis(50),
                stay_alive: true,
                migration: Some(migration),
                coordinator,
                ..AgentConfig::default()
            },
        )
        .await
    });
    let (resumed, client_chan2) = tokio::time::timeout(
        TIMEOUT,
        client_resume_handshake(
            &client_conn2,
            PASSWORD,
            "e2e-client",
            &key,
            &client1.welcome,
            first_header.frame_id.into(),
            u64::from(first_header.frame_seq),
        ),
    )
    .await
    .context("live migration timeout")??;
    assert_eq!(resumed.session_id, client1.welcome.session_id);

    let mut saw_keyframe = false;
    let mut resumed_header = None;
    for _ in 0..20 {
        let datagram = tokio::time::timeout(TIMEOUT, client_conn2.read_datagram())
            .await
            .context("resumed stream timeout")??;
        let (header, _) = decode_video(&datagram)?;
        saw_keyframe = header.is_key();
        resumed_header = Some(header);
        if saw_keyframe {
            break;
        }
    }
    let resumed_header = resumed_header.context("no resumed video frame")?;
    assert!(saw_keyframe, "COMMIT did not request a keyframe");
    assert!(resumed_header.frame_id > first_header.frame_id);

    let (mut tx2, _rx2) = client_chan2.split();
    tx2.send_ctrl(&dfy_proto::Envelope::bye(dfy_proto::BYE_NORMAL))
        .await?;
    client_conn1.close(0u32.into(), b"test done");
    client_conn2.close(0u32.into(), b"test done");

    let first_stats = tokio::time::timeout(TIMEOUT, first_agent)
        .await
        .context("first agent session timeout")?
        .context("first agent task failed")??;
    let second_stats = tokio::time::timeout(TIMEOUT, second_agent)
        .await
        .context("second agent session timeout")?
        .context("second agent task failed")??;
    stop_tx
        .send(())
        .map_err(|_| anyhow::anyhow!("capture owner stopped"))?;
    capture_owner.await??;
    acceptor.await?;
    first_client.await?;
    assert_eq!(first_stats.end_reason, "migrated");
    assert!(first_stats.frames_sent > 0);
    // The old owner may send its own initial decoder keyframe, but never the COMMIT keyframe.
    assert!(first_stats.keyframes_sent <= 1);
    assert!(second_stats.frames_sent > 0);
    assert!(second_stats.keyframes_sent > 0);
    assert!(!keyframe_requested.load(Ordering::Acquire));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_feedback_degrades_and_recovers_encoder() -> Result<()> {
    install_crypto_provider();
    let (endpoint, identity, addr) = bind_agent_random()?;
    let builds = Arc::new(StdMutex::new(Vec::<RuntimeSettings>::new()));
    let agent_builds = Arc::clone(&builds);
    let agent = tokio::spawn(async move {
        let incoming = endpoint.accept().await.expect("endpoint closed");
        let conn = incoming.await?;
        let mut auth = ServerAuth::new("test-password", "adaptive-agent");
        auth.permission_bitmap = PERM_VIEW | dfy_proto::PERM_CONTROL;
        let (welcome, chan) = server_handshake(&conn, &auth).await?;
        let initial = RuntimeSettings {
            fps: 120,
            bitrate_kbps: 8000,
            resolution: Some(dfy_session::EncodeResolution {
                width: 320,
                height: 180,
            }),
        };
        let mut capture = start_capture_loop_with_encoder_factory(
            CaptureLoopConfig {
                capture_timeout_ms: 1,
                max_fps: 120,
                queue_capacity: 64,
                max_consecutive_errors: 3,
            },
            initial,
            move || {
                Ok((
                    FakeCapturer::new(320, 180, u64::MAX),
                    FakeEncoder::with_settings(initial),
                ))
            },
            move |settings| {
                agent_builds.lock().unwrap().push(*settings);
                Ok(FakeEncoder::with_settings(*settings))
            },
        )?;
        let tuning = dfy_path::AdaptationConfig {
            congestion_samples: 2,
            recovery_samples: 2,
            min_change_interval_ms: 0,
            recovery_cooldown_ms: 0,
            ..dfy_path::AdaptationConfig::default()
        };
        let stats = serve_agent(
            conn,
            welcome,
            chan,
            &mut capture,
            IgnoreHandler,
            AgentConfig {
                stay_alive: true,
                adaptation: Some(StreamAdaptationConfig { tuning }),
                ..AgentConfig::default()
            },
        )
        .await?;
        anyhow::Ok((stats, capture.runtime_control().current()))
    });

    let (_client_endpoint, conn) = connect(addr, client_tls(Some(identity.fingerprint))?).await?;
    let (_welcome, chan) =
        client_handshake(&conn, "test-password", "adaptive-client", vec![4; 32]).await?;
    let (mut tx, mut rx) = chan.split();
    let first = tokio::time::timeout(Duration::from_secs(5), conn.read_datagram()).await??;
    let (header, _) = decode_video(&first)?;
    assert!(header.is_key());
    let initial = recv_video_config(&mut tx, &mut rx).await?;
    assert_eq!(
        (
            initial.width,
            initial.height,
            initial.fps,
            initial.bitrate_kbps
        ),
        (320, 180, 120, 8_000)
    );

    for _ in 0..2 {
        tx.send_ctrl(&Envelope::stream_feedback(StreamFeedback {
            window_ms: 500,
            payload_bytes: 1000,
            packets: 2,
            frames_assembled: 2,
            frames_delivered: 2,
            dropped_frames: 0,
            stall_ms: 1_200,
        }))
        .await?;
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let degraded = recv_video_config(&mut tx, &mut rx).await?;
    assert!(degraded.fps < 120);
    assert!(degraded.bitrate_kbps < 8000);
    assert!(degraded.width < 320 || degraded.height < 180);
    let mut rebuilt_keyframe = false;
    for _ in 0..64 {
        let bytes = tokio::time::timeout(Duration::from_secs(2), conn.read_datagram()).await??;
        let (header, _) = decode_video(&bytes)?;
        if header.is_key() {
            rebuilt_keyframe = true;
            break;
        }
    }
    assert!(
        rebuilt_keyframe,
        "resolution rebuild must produce a keyframe"
    );

    for _ in 0..2 {
        tx.send_ctrl(&Envelope::stream_feedback(StreamFeedback {
            window_ms: 500,
            payload_bytes: 10_000_000,
            packets: 100,
            frames_assembled: 60,
            frames_delivered: 60,
            dropped_frames: 0,
            stall_ms: 0,
        }))
        .await?;
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let recovered = recv_video_config(&mut tx, &mut rx).await?;
    assert!(recovered.fps > degraded.fps);
    assert!(recovered.bitrate_kbps > degraded.bitrate_kbps);
    assert!(recovered.fps <= 120);
    assert!(recovered.bitrate_kbps <= 8000);
    assert!(recovered.width <= 320 && recovered.height <= 180);

    tx.send_ctrl(&Envelope::quality_control(160, 90, 30, 2_000))
        .await?;
    let limited = recv_video_config(&mut tx, &mut rx).await?;
    assert_eq!(
        (
            limited.width,
            limited.height,
            limited.fps,
            limited.bitrate_kbps
        ),
        (160, 90, 30, 2_000)
    );
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    tx.send_ctrl(&Envelope::quality_control(0, 0, 0, 0)).await?;
    let restored = recv_video_config(&mut tx, &mut rx).await?;
    assert_eq!(
        (
            restored.width,
            restored.height,
            restored.fps,
            restored.bitrate_kbps
        ),
        (320, 180, 120, 8_000)
    );

    tx.send_ctrl(&Envelope::bye(dfy_proto::BYE_NORMAL)).await?;
    let completed = tokio::time::timeout(Duration::from_secs(10), agent).await??;
    let (stats, current) = completed?;
    assert_eq!(stats.adaptation_changes, 2);
    assert_eq!(stats.quality_controls, 2);
    assert_eq!(stats.quality_control_errors, 0);
    assert!(stats.feedback_windows >= 4);
    assert_eq!(current.fps, restored.fps);
    assert_eq!(current.bitrate_kbps, restored.bitrate_kbps);
    assert_eq!(builds.lock().unwrap().len(), 4);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fatal_quality_confirmation_stops_stay_alive_capture() -> Result<()> {
    install_crypto_provider();
    let (endpoint, identity, addr) = bind_agent_random()?;
    let calls = Arc::new(AtomicUsize::new(0));
    let agent_calls = Arc::clone(&calls);
    let closure_calls = Arc::clone(&agent_calls);
    let transitions = Arc::new(StdMutex::new(Vec::<bool>::new()));
    let observed_transitions = Arc::clone(&transitions);
    let observer_transitions = Arc::clone(&observed_transitions);
    let (first_done_tx, first_done_rx) = oneshot::channel();
    let agent = tokio::spawn(async move {
        let initial = RuntimeSettings {
            fps: 120,
            bitrate_kbps: 8_000,
            resolution: Some(dfy_session::EncodeResolution {
                width: 320,
                height: 180,
            }),
        };
        let (mut capture, _input) = CaptureLoop::external_with_runtime_and_source(
            1,
            initial,
            initial.resolution,
            move |_| {
                let call = closure_calls.fetch_add(1, Ordering::SeqCst);
                if call == 0 {
                    Ok(RuntimeSettings {
                        fps: 30,
                        bitrate_kbps: 2_000,
                        resolution: Some(dfy_session::EncodeResolution {
                            width: 320,
                            height: 180,
                        }),
                    })
                } else {
                    anyhow::bail!("injected rollback failure")
                }
            },
        )?;
        capture.set_capture_activity_observer(move |active| {
            observer_transitions.lock().unwrap().push(active);
        });
        let config = AgentConfig {
            stay_alive: true,
            adaptation: Some(StreamAdaptationConfig::default()),
            ..AgentConfig::default()
        };

        let incoming = endpoint.accept().await.context("first accept")?;
        let conn = incoming.await?;
        let mut auth = ServerAuth::new("test-password", "fatal-quality-agent");
        auth.permission_bitmap = PERM_VIEW | PERM_CONTROL;
        let (welcome, chan) = server_handshake(&conn, &auth).await?;
        let first = serve_agent(
            conn,
            welcome,
            chan,
            &mut capture,
            IgnoreHandler,
            config.clone(),
        )
        .await?;
        let stopping = capture.is_stopping();
        first_done_tx
            .send(())
            .map_err(|_| anyhow::anyhow!("client left"))?;

        let incoming = endpoint.accept().await.context("second accept")?;
        let conn = incoming.await?;
        let (welcome, chan) = server_handshake(&conn, &auth).await?;
        let second = serve_agent(conn, welcome, chan, &mut capture, IgnoreHandler, config).await?;
        anyhow::Ok((
            first,
            second,
            stopping,
            agent_calls.load(Ordering::SeqCst),
            observed_transitions,
        ))
    });

    let tls = client_tls(Some(identity.fingerprint))?;
    let (_client_ep, conn) = connect(addr, tls).await?;
    let (_welcome, chan) =
        client_handshake(&conn, "test-password", "fatal-quality-client", vec![4; 32]).await?;
    let (mut tx, _rx) = chan.split();
    tx.send_ctrl(&Envelope::quality_control(160, 90, 30, 2_000))
        .await?;
    tokio::time::timeout(Duration::from_secs(10), first_done_rx)
        .await?
        .context("first session did not finish")?;
    drop(conn);
    drop(tx);

    let tls = client_tls(Some(identity.fingerprint))?;
    let (_client_ep2, conn2) = connect(addr, tls).await?;
    let (_welcome2, _chan2) = client_handshake(
        &conn2,
        "test-password",
        "fatal-quality-client-2",
        vec![5; 32],
    )
    .await?;
    let (first, second, stopping, apply_calls, transitions) =
        tokio::time::timeout(Duration::from_secs(10), agent)
            .await?
            .context("agent task failed")??;
    assert_eq!(first.end_reason, "quality_confirmation_failed");
    assert_eq!(first.quality_controls, 1);
    assert_eq!(first.quality_control_errors, 1);
    assert!(stopping);
    assert_eq!(second.end_reason, "capture_stopping");
    assert_eq!(second.frames_sent, 0);
    assert_eq!(apply_calls, 2);
    assert_eq!(*transitions.lock().unwrap(), vec![true, false]);
    Ok(())
}

async fn recv_resume_ack(
    rx: &mut dfy_transport::CtrlReceiver,
) -> Result<dfy_proto::SessionResumeOk> {
    loop {
        let env = tokio::time::timeout(Duration::from_secs(5), rx.recv_ctrl())
            .await??
            .context("control closed before resume ack")?;
        match env.body {
            Some(envelope::Body::SessionResumeOk(ack)) => return Ok(ack),
            Some(envelope::Body::Ping(_)) | Some(envelope::Body::Pong(_)) => {}
            _ => anyhow::bail!("expected SessionResumeOk"),
        }
    }
}

async fn recv_video_config(
    tx: &mut dfy_transport::CtrlSender,
    rx: &mut dfy_transport::CtrlReceiver,
) -> Result<dfy_proto::VideoConfig> {
    loop {
        let env = tokio::time::timeout(Duration::from_secs(5), rx.recv_ctrl())
            .await??
            .context("control closed")?;
        match env.body {
            Some(envelope::Body::VideoConfig(config)) => return Ok(config),
            Some(envelope::Body::Ping(ping)) => tx.send_ctrl(&Envelope::pong(ping.ts_us)).await?,
            _ => {}
        }
    }
}

struct NonKeyframeEncoder {
    keyframe_requested: Arc<AtomicBool>,
}

impl NonKeyframeEncoder {
    fn new(keyframe_requested: Arc<AtomicBool>) -> Self {
        Self { keyframe_requested }
    }
}

impl Encoder for NonKeyframeEncoder {
    fn encode(&mut self, frame: &RawFrame) -> anyhow::Result<Vec<EncodedFrame>> {
        let keyframe = self.keyframe_requested.swap(false, Ordering::AcqRel);
        Ok(vec![EncodedFrame {
            data: b"DFYF".to_vec(),
            keyframe,
            pts_us: frame.pts_us,
        }])
    }

    fn request_keyframe(&mut self) {
        self.keyframe_requested.store(true, Ordering::Release);
    }

    fn flush(&mut self) -> anyhow::Result<Vec<EncodedFrame>> {
        Ok(Vec::new())
    }
}

async fn send_and_receive(
    lease: &SessionLease,
    sender: &quinn::Connection,
    receiver: &quinn::Connection,
    expected: u32,
) -> Result<VideoHeader> {
    let sent = lease
        .send_frame(sender, expected, &[expected as u8], true)?
        .context("lease did not send")?;
    assert_eq!(sent.packets, 1);
    let datagram = tokio::time::timeout(Duration::from_secs(2), receiver.read_datagram()).await??;
    let (header, _) = decode_video(&datagram)?;
    assert_eq!(header.frame_id, expected);
    assert_eq!(header.frame_seq, expected as u16);
    assert!(header.is_key());
    Ok(header)
}

fn signed_prepare(
    key: &SigningKey,
    hello: &dfy_proto::Hello,
    exporter: &[u8; 32],
    welcome: &dfy_proto::Welcome,
) -> Result<SessionResume> {
    let mut request = SessionResume {
        last_frame_id: 0,
        last_frame_seq: 0,
        session_id: welcome.session_id.clone(),
        resume_token: welcome.resume_token.clone(),
        phase: SESSION_RESUME_PREPARE,
        client_nonce: vec![9; 32],
        server_nonce: Vec::new(),
        signature: Vec::new(),
        permission_bitmap: PERM_VIEW,
    };
    request.signature = dfy_transport::resume::sign_request(key, hello, exporter, &request)?;
    Ok(request)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_new_active_session_requests_a_decoder_keyframe() -> Result<()> {
    install_crypto_provider();
    let (endpoint, identity, addr) = bind_agent_random()?;
    let auth = ServerAuth::new("test-password", "keyframe-agent");
    let coordinator = SessionCoordinator::default();
    let agent = tokio::spawn(async move {
        let keyframe_requested = Arc::new(AtomicBool::new(false));
        let factory_flag = Arc::clone(&keyframe_requested);
        let mut capture = start_capture_loop_with_encoder_factory(
            CaptureLoopConfig {
                capture_timeout_ms: 1,
                max_fps: 120,
                queue_capacity: 64,
                max_consecutive_errors: 3,
            },
            RuntimeSettings {
                fps: 120,
                bitrate_kbps: 4000,
                resolution: Some(dfy_session::EncodeResolution {
                    width: 320,
                    height: 180,
                }),
            },
            move || {
                Ok((
                    FakeCapturer::new(320, 180, u64::MAX),
                    NonKeyframeEncoder::new(Arc::clone(&factory_flag)),
                ))
            },
            move |_settings| Ok(NonKeyframeEncoder::new(Arc::clone(&keyframe_requested))),
        )?;
        let mut stats = Vec::new();
        for _ in 0..2 {
            let incoming = endpoint.accept().await.expect("endpoint closed");
            let conn = incoming.await?;
            let handshake = server_handshake_context(&conn, &auth).await?;
            let migration = MigrationContext {
                auth: auth.clone(),
                hello: handshake.hello.clone(),
                exporter: handshake.exporter,
                coordinator: coordinator.clone(),
            };
            stats.push(
                serve_agent(
                    conn,
                    handshake.welcome,
                    handshake.chan,
                    &mut capture,
                    IgnoreHandler,
                    AgentConfig {
                        stay_alive: true,
                        migration: Some(migration),
                        coordinator: coordinator.clone(),
                        ..AgentConfig::default()
                    },
                )
                .await?,
            );
        }
        capture.stop();
        capture.join();
        anyhow::Ok(stats)
    });

    for _ in 0..2 {
        let (_client_endpoint, conn) =
            connect(addr, client_tls(Some(identity.fingerprint))?).await?;
        let (_welcome, chan) =
            client_handshake(&conn, "test-password", "keyframe-client", vec![4; 32]).await?;
        let (mut tx, _rx) = chan.split();
        let mut saw_keyframe = false;
        for index in 0..128 {
            let bytes =
                tokio::time::timeout(Duration::from_secs(2), conn.read_datagram()).await??;
            let (header, _) = decode_video(&bytes)?;
            if index == 0 {
                assert!(
                    header.is_key(),
                    "fresh session received a stale non-keyframe before its keyframe"
                );
            }
            if header.is_key() {
                saw_keyframe = true;
                break;
            }
        }
        assert!(
            saw_keyframe,
            "new active session did not receive a keyframe"
        );
        tx.send_ctrl(&Envelope::bye(dfy_proto::BYE_NORMAL)).await?;
        conn.close(0u32.into(), b"test done");
    }

    let stats = tokio::time::timeout(Duration::from_secs(10), agent).await???;
    assert_eq!(stats.len(), 2);
    assert!(stats.iter().all(|session| session.keyframes_sent > 0));
    Ok(())
}
