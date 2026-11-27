//! 真实 loopback QUIC 回归：不启动桌面采集/输入或剪贴板，不访问外网。
//! 攻击端独立编码认证 transcript，以免仅复测生产 helper 的同一实现。

use std::{net::SocketAddr, time::Duration};

use anyhow::{Context, Result};
use argon2::{Algorithm, Argon2, Params, Version};
use dfy_proto::{
    envelope, Challenge, Envelope, Hello, ERR_AUTH, ERR_PROTOCOL, PERM_VIEW, PROTO_MAJOR,
    PROTO_MINOR,
};
use dfy_transport::{
    bind_agent_random, client_handshake, client_tls, connect, server_handshake, CtrlChannel,
    ServerAuth,
};
use hmac::{Hmac, Mac};
use quinn::{Connection, Endpoint};
use sha2::Sha256;
use tokio::sync::Mutex;

const PASSWORD: &str = "local-test-password";
const TEST_TIMEOUT: Duration = Duration::from_secs(20);
// 多个测试同时发起正常握手会触发真实全局 KDF 过载保护。串行化这些安全场景。
static TEST_LOCK: Mutex<()> = Mutex::const_new(());

struct Pair {
    client: Connection,
    server: Connection,
    client_ep: Endpoint,
    server_ep: Endpoint,
}

impl Drop for Pair {
    fn drop(&mut self) {
        self.client_ep.close(0u32.into(), b"test done");
        self.server_ep.close(0u32.into(), b"test done");
    }
}

impl Pair {
    async fn new(pin: bool) -> Result<Self> {
        let (server_ep, identity, addr) = bind_agent_random()?;
        let tls = client_tls(pin.then_some(identity.fingerprint()))?;
        let (server, client) = tokio::time::timeout(TEST_TIMEOUT, async {
            tokio::join!(accept(&server_ep), connect(addr, tls))
        })
        .await?;
        let (client_ep, client) = client?;
        Ok(Self {
            client,
            server: server?,
            client_ep,
            server_ep,
        })
    }
}

async fn accept(endpoint: &Endpoint) -> Result<Connection> {
    endpoint
        .accept()
        .await
        .context("test endpoint closed")?
        .await
        .context("test QUIC TLS accept failed")
}

async fn open(conn: &Connection) -> Result<CtrlChannel> {
    let (send, recv) = conn.open_bi().await?;
    Ok(CtrlChannel::new(send, recv, conn.clone()))
}

async fn accept_ctrl(conn: &Connection) -> Result<CtrlChannel> {
    let (send, recv) = conn.accept_bi().await?;
    Ok(CtrlChannel::new(send, recv, conn.clone()))
}

fn hello() -> Hello {
    Hello {
        proto_major: PROTO_MAJOR,
        proto_minor: PROTO_MINOR,
        client_name: "attack-test-client".into(),
        device_pubkey: vec![8; 32],
        client_nonce: vec![9; 32],
        resume_session_id: Vec::new(),
    }
}

fn hello_frame(hello: Hello) -> Envelope {
    Envelope {
        body: Some(envelope::Body::Hello(hello)),
    }
}

fn challenge_frame(challenge: Challenge) -> Envelope {
    Envelope {
        body: Some(envelope::Body::Challenge(challenge)),
    }
}

async fn recv_challenge(chan: &mut CtrlChannel) -> Result<Challenge> {
    match chan.recv_ctrl().await?.context("missing Challenge")?.body {
        Some(envelope::Body::Challenge(challenge)) => Ok(challenge),
        other => anyhow::bail!("expected Challenge, got {other:?}"),
    }
}

async fn assert_auth_reject(chan: &mut CtrlChannel) -> Result<()> {
    let message = chan.recv_ctrl().await?.context("missing Reject")?;
    assert!(matches!(
        message.body,
        Some(envelope::Body::Reject(reject)) if reject.code == ERR_AUTH
    ));
    Ok(())
}

async fn assert_no_auth(chan: &mut CtrlChannel) {
    let message = tokio::time::timeout(Duration::from_secs(3), chan.recv_ctrl())
        .await
        .expect("client must stop handshake promptly");
    if let Ok(Some(message)) = message {
        assert!(
            !matches!(message.body, Some(envelope::Body::Auth(_))),
            "client disclosed its proof before verifying the server"
        );
        panic!("client should close the rejected authentication channel");
    }
}

fn exporter(conn: &Connection) -> [u8; 32] {
    let mut output = [0; 32];
    conn.export_keying_material(&mut output, b"EXPORTER-DFYP-1-AUTH-V2", b"")
        .expect("test TLS exporter");
    output
}

fn lp(out: &mut Vec<u8>, value: &[u8]) {
    out.extend_from_slice(&(value.len() as u32).to_be_bytes());
    out.extend_from_slice(value);
}

// 独立实现冻结的线格式，以真实 exporter 为输入。
fn proof(
    password_key: &[u8; 32],
    role: &[u8],
    hello: &Hello,
    c: &Challenge,
    e: &[u8; 32],
) -> Vec<u8> {
    let mut transcript = Vec::new();
    for field in [
        b"DFYP-AUTH-V2".as_slice(),
        &c.auth_version.to_be_bytes(),
        &hello.proto_major.to_be_bytes(),
        &hello.proto_minor.to_be_bytes(),
        hello.client_name.as_bytes(),
        &hello.device_pubkey,
        &hello.client_nonce,
        &c.nonce,
        &c.salt,
        b"client",
        b"server",
        e,
    ] {
        lp(&mut transcript, field);
    }
    let mut input = Vec::new();
    lp(&mut input, role);
    lp(&mut input, &transcript);
    let mut mac = Hmac::<Sha256>::new_from_slice(password_key).unwrap();
    mac.update(&input);
    mac.finalize().into_bytes().to_vec()
}

async fn key(password: &str, salt: &[u8]) -> [u8; 32] {
    let password = password.to_owned();
    let salt = salt.to_vec();
    tokio::task::spawn_blocking(move || {
        let mut key = [0; 32];
        Argon2::new(
            Algorithm::Argon2id,
            Version::V0x13,
            Params::new(19_456, 2, 1, Some(32)).unwrap(),
        )
        .hash_password_into(password.as_bytes(), &salt, &mut key)
        .unwrap();
        key
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn correct_password_and_matching_pin_establish_view_session() -> Result<()> {
    let _serial = TEST_LOCK.lock().await;
    let pair = Pair::new(true).await?;
    assert_eq!(exporter(&pair.client), exporter(&pair.server));
    let auth = ServerAuth::new(PASSWORD, "verified-agent");
    let (server, client) = tokio::time::timeout(TEST_TIMEOUT, async {
        tokio::join!(
            server_handshake(&pair.server, &auth),
            client_handshake(&pair.client, PASSWORD, "verified-client", vec![1; 32]),
        )
    })
    .await?;
    let (server_welcome, mut server_chan) = server?;
    let (client_welcome, mut client_chan) = client?;
    assert_eq!(server_welcome, client_welcome);
    assert_eq!(client_welcome.permission_bitmap, PERM_VIEW);
    client_chan.send_ctrl(&Envelope::ping(1234)).await?;
    let ping = server_chan.recv_ctrl().await?.context("missing ping")?;
    assert!(matches!(ping.body, Some(envelope::Body::Ping(ping)) if ping.ts_us == 1234));
    server_chan.send_ctrl(&Envelope::pong(1234)).await?;
    assert!(matches!(
        client_chan.recv_ctrl().await?.context("missing pong")?.body,
        Some(envelope::Body::Pong(pong)) if pong.ts_us == 1234
    ));
    Ok(())
}

#[tokio::test]
async fn wrong_password_fails_server_authentication() -> Result<()> {
    let _serial = TEST_LOCK.lock().await;
    let pair = Pair::new(false).await?;
    let auth = ServerAuth::new(PASSWORD, "agent");
    let (server, client) = tokio::time::timeout(TEST_TIMEOUT, async {
        tokio::join!(
            server_handshake(&pair.server, &auth),
            client_handshake(&pair.client, "wrong-password", "client", vec![1; 32]),
        )
    })
    .await?;
    assert!(server.is_err());
    let error = client.err().context("wrong password unexpectedly passed")?;
    assert!(error.to_string().contains("服务端口令或通道绑定证明"));
    Ok(())
}

#[tokio::test]
async fn wrong_client_password_proof_is_rejected() -> Result<()> {
    let _serial = TEST_LOCK.lock().await;
    let pair = Pair::new(false).await?;
    let auth = ServerAuth::new(PASSWORD, "agent");
    let (server, attack) = tokio::time::timeout(TEST_TIMEOUT, async {
        tokio::join!(server_handshake(&pair.server, &auth), async {
            let mut chan = open(&pair.client).await?;
            let hello = hello();
            chan.send_ctrl(&hello_frame(hello.clone())).await?;
            let challenge = recv_challenge(&mut chan).await?;
            let wrong_key = key("wrong-password", &challenge.salt).await;
            let wrong_proof = proof(
                &wrong_key,
                b"client-proof",
                &hello,
                &challenge,
                &exporter(&pair.client),
            );
            chan.send_ctrl(&Envelope::auth(wrong_proof)).await?;
            assert_auth_reject(&mut chan).await
        })
    })
    .await?;
    attack?;
    assert!(server.is_err());
    Ok(())
}

#[tokio::test]
async fn tls_terminating_relay_cannot_forward_authentication() -> Result<()> {
    let _serial = TEST_LOCK.lock().await;
    // client -> malicious relay 与 malicious relay -> agent 各终结一次 TLS。
    let downstream = Pair::new(false).await?;
    let upstream = Pair::new(false).await?;
    assert_ne!(exporter(&downstream.server), exporter(&upstream.client));
    let auth = ServerAuth::new(PASSWORD, "real-agent");
    let (client, server, relay) = tokio::time::timeout(TEST_TIMEOUT, async {
        tokio::join!(
            client_handshake(&downstream.client, PASSWORD, "victim", vec![2; 32]),
            server_handshake(&upstream.server, &auth),
            async {
                let mut victim = accept_ctrl(&downstream.server).await?;
                let mut agent = open(&upstream.client).await?;
                let hello = victim.recv_ctrl().await?.context("relay missing Hello")?;
                agent.send_ctrl(&hello).await?;
                let challenge = agent
                    .recv_ctrl()
                    .await?
                    .context("relay missing Challenge")?;
                assert!(matches!(challenge.body, Some(envelope::Body::Challenge(_))));
                victim.send_ctrl(&challenge).await?;
                // 透明转发在这里终止：victim 已拒绝另一个 TLS 通道的 server-proof。
                assert_no_auth(&mut victim).await;
                upstream
                    .client
                    .close(ERR_AUTH.into(), b"no client proof to relay");
                Ok::<_, anyhow::Error>(())
            }
        )
    })
    .await?;
    relay?;
    assert!(server.is_err(), "relay authenticated to real agent");
    let error = client.err().context("victim accepted terminating relay")?;
    assert!(error.to_string().contains("服务端口令或通道绑定证明"));
    Ok(())
}

#[tokio::test]
async fn server_proof_from_different_exporter_is_rejected_before_auth() -> Result<()> {
    let _serial = TEST_LOCK.lock().await;
    let pair = Pair::new(false).await?;
    let other = Pair::new(false).await?;
    assert_ne!(exporter(&pair.server), exporter(&other.server));
    let (client, attack) = tokio::time::timeout(TEST_TIMEOUT, async {
        tokio::join!(
            client_handshake(&pair.client, PASSWORD, "client", vec![1; 32]),
            async {
                let mut chan = accept_ctrl(&pair.server).await?;
                let hello = match chan.recv_ctrl().await?.context("missing Hello")?.body {
                    Some(envelope::Body::Hello(hello)) => hello,
                    _ => anyhow::bail!("expected Hello"),
                };
                let mut challenge = Challenge {
                    nonce: vec![3; 32],
                    salt: vec![4; 16],
                    server_proof: Vec::new(),
                    auth_version: 2,
                };
                let key = key(PASSWORD, &challenge.salt).await;
                challenge.server_proof = proof(
                    &key,
                    b"server-proof",
                    &hello,
                    &challenge,
                    &exporter(&other.server),
                );
                // Hello、双方 nonce、salt、角色全部相同，唯一差异是 exporter。
                assert_ne!(
                    challenge.server_proof,
                    proof(
                        &key,
                        b"server-proof",
                        &hello,
                        &challenge,
                        &exporter(&pair.server)
                    )
                );
                chan.send_ctrl(&challenge_frame(challenge)).await?;
                assert_no_auth(&mut chan).await;
                Ok::<_, anyhow::Error>(())
            }
        )
    })
    .await?;
    attack?;
    assert!(client.is_err());
    Ok(())
}

#[tokio::test]
async fn client_proof_from_different_exporter_is_rejected() -> Result<()> {
    let _serial = TEST_LOCK.lock().await;
    let pair = Pair::new(false).await?;
    let other = Pair::new(false).await?;
    assert_ne!(exporter(&pair.client), exporter(&other.client));
    let auth = ServerAuth::new(PASSWORD, "agent");
    let (server, attack) = tokio::time::timeout(TEST_TIMEOUT, async {
        tokio::join!(server_handshake(&pair.server, &auth), async {
            let mut chan = open(&pair.client).await?;
            let hello = hello();
            chan.send_ctrl(&hello_frame(hello.clone())).await?;
            let challenge = recv_challenge(&mut chan).await?;
            let key = key(PASSWORD, &challenge.salt).await;
            let replay = proof(
                &key,
                b"client-proof",
                &hello,
                &challenge,
                &exporter(&other.client),
            );
            chan.send_ctrl(&Envelope::auth(replay)).await?;
            assert_auth_reject(&mut chan).await
        })
    })
    .await?;
    attack?;
    assert!(server.is_err());
    Ok(())
}

#[tokio::test]
async fn server_proof_cannot_be_reflected_as_client_proof() -> Result<()> {
    let _serial = TEST_LOCK.lock().await;
    let pair = Pair::new(false).await?;
    let auth = ServerAuth::new(PASSWORD, "agent");
    let (server, attack) = tokio::time::timeout(TEST_TIMEOUT, async {
        tokio::join!(server_handshake(&pair.server, &auth), async {
            let mut chan = open(&pair.client).await?;
            chan.send_ctrl(&hello_frame(hello())).await?;
            let challenge = recv_challenge(&mut chan).await?;
            chan.send_ctrl(&Envelope::auth(challenge.server_proof))
                .await?;
            assert_auth_reject(&mut chan).await
        })
    })
    .await?;
    attack?;
    assert!(server.is_err());
    Ok(())
}

#[tokio::test]
async fn client_role_proof_is_not_accepted_as_server_proof() -> Result<()> {
    let _serial = TEST_LOCK.lock().await;
    let pair = Pair::new(false).await?;
    let (client, attack) = tokio::time::timeout(TEST_TIMEOUT, async {
        tokio::join!(
            client_handshake(&pair.client, PASSWORD, "client", vec![1; 32]),
            async {
                let mut chan = accept_ctrl(&pair.server).await?;
                let hello = match chan.recv_ctrl().await?.context("missing Hello")?.body {
                    Some(envelope::Body::Hello(hello)) => hello,
                    _ => anyhow::bail!("expected Hello"),
                };
                let mut challenge = Challenge {
                    nonce: vec![3; 32],
                    salt: vec![4; 16],
                    server_proof: Vec::new(),
                    auth_version: 2,
                };
                let key = key(PASSWORD, &challenge.salt).await;
                challenge.server_proof = proof(
                    &key,
                    b"client-proof",
                    &hello,
                    &challenge,
                    &exporter(&pair.server),
                );
                chan.send_ctrl(&challenge_frame(challenge)).await?;
                assert_no_auth(&mut chan).await;
                Ok::<_, anyhow::Error>(())
            }
        )
    })
    .await?;
    attack?;
    assert!(client.is_err());
    Ok(())
}

#[tokio::test]
async fn missing_auth_version_and_malformed_challenges_never_fall_back() -> Result<()> {
    let _serial = TEST_LOCK.lock().await;
    for challenge in [
        Challenge {
            nonce: vec![3; 32],
            salt: vec![4; 16],
            server_proof: vec![0; 32],
            auth_version: 0,
        },
        Challenge {
            nonce: vec![3; 31],
            salt: vec![4; 16],
            server_proof: vec![0; 32],
            auth_version: 2,
        },
        Challenge {
            nonce: vec![3; 32],
            salt: vec![4; 15],
            server_proof: vec![0; 32],
            auth_version: 2,
        },
        Challenge {
            nonce: vec![3; 32],
            salt: vec![4; 16],
            server_proof: vec![0; 31],
            auth_version: 2,
        },
    ] {
        let pair = Pair::new(false).await?;
        let (client, attack) = tokio::time::timeout(TEST_TIMEOUT, async {
            tokio::join!(
                client_handshake(&pair.client, PASSWORD, "client", vec![1; 32]),
                async {
                    let mut chan = accept_ctrl(&pair.server).await?;
                    chan.recv_ctrl().await?.context("missing Hello")?;
                    chan.send_ctrl(&challenge_frame(challenge)).await?;
                    assert_no_auth(&mut chan).await;
                    Ok::<_, anyhow::Error>(())
                }
            )
        })
        .await?;
        attack?;
        assert!(client.is_err());
    }
    Ok(())
}

#[tokio::test]
async fn legacy_hello_without_client_nonce_is_rejected() -> Result<()> {
    let _serial = TEST_LOCK.lock().await;
    let pair = Pair::new(false).await?;
    let auth = ServerAuth::new(PASSWORD, "agent");
    let (server, attack) = tokio::time::timeout(TEST_TIMEOUT, async {
        tokio::join!(server_handshake(&pair.server, &auth), async {
            let mut chan = open(&pair.client).await?;
            chan.send_ctrl(&Envelope::hello(
                PROTO_MAJOR,
                PROTO_MINOR,
                "legacy",
                vec![1; 32],
            ))
            .await?;
            let response = chan.recv_ctrl().await?.context("missing legacy Reject")?;
            assert!(
                matches!(response.body, Some(envelope::Body::Reject(r)) if r.code == ERR_PROTOCOL)
            );
            Ok::<_, anyhow::Error>(())
        })
    })
    .await?;
    attack?;
    assert!(server.is_err());
    Ok(())
}

#[tokio::test]
async fn certificate_pin_mismatch_fails_quic_tls() -> Result<()> {
    let _serial = TEST_LOCK.lock().await;
    let (server_ep, identity, addr): (_, _, SocketAddr) = bind_agent_random()?;
    let mut wrong_pin = identity.fingerprint();
    wrong_pin[0] ^= 1;
    let (server, client) = tokio::time::timeout(TEST_TIMEOUT, async {
        tokio::join!(
            accept(&server_ep),
            connect(addr, client_tls(Some(wrong_pin)).unwrap())
        )
    })
    .await?;
    assert!(client.is_err(), "mismatched certificate pin was accepted");
    assert!(server.is_err(), "server completed mismatched TLS handshake");
    server_ep.close(0u32.into(), b"test done");
    Ok(())
}
