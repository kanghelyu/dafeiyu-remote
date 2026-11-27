// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors
//! Adversarial loopback tests for relay admission and endpoint ownership.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use dfy_relay::auth::compute_auth;
use dfy_relay::config::{Config, Limits, LogLevel, RelayKey};
use dfy_relay::proto::{self, PacketClass, Tag, CHAN_CTRL, CHAN_DATA, CHAN_PING};
use dfy_relay::relay::{self, RelayHandle};
use dfy_relay::state::{ROLE_INIT, ROLE_RESP};
use dfy_relay::unix_ms;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::time::timeout;

const KEY: [u8; 32] = [0x71; 32];
const WAIT: Duration = Duration::from_secs(2);
const QUIET: Duration = Duration::from_millis(150);

fn config() -> Config {
    Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        tcp_bind: Some("127.0.0.1:0".parse().unwrap()),
        admin_addr: None,
        key: RelayKey::from_bytes(&KEY).unwrap(),
        log: LogLevel::Off,
        log_json: false,
        no_log_ips: true,
        limits: Limits {
            per_tag_bytes_per_s: None,
            global_bytes_per_s: None,
            rebind_guard: Duration::ZERO,
            ..Limits::default()
        },
    }
}

fn bind_packet(tag: &Tag, role: u8, nonce: u8) -> Vec<u8> {
    let nonce = [nonce; 16];
    let ts = unix_ms();
    let auth = compute_auth(&KEY, tag, role, &nonce, ts);
    proto::bind_packet(tag, role, &nonce, ts, &auth)
}

async fn udp_recv(socket: &UdpSocket, relay: &RelayHandle) -> Vec<u8> {
    let mut buf = [0; 2048];
    let (n, from) = timeout(WAIT, socket.recv_from(&mut buf))
        .await
        .expect("UDP receive timed out")
        .unwrap();
    assert_eq!(from, relay.udp_local);
    buf[..n].to_vec()
}

async fn udp_bind(socket: &UdpSocket, relay: &RelayHandle, packet: &[u8]) {
    socket.send_to(packet, relay.udp_local).await.unwrap();
    assert_eq!(
        proto::classify(&udp_recv(socket, relay).await),
        PacketClass::BindOk
    );
}

async fn tcp_frame(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    timeout(WAIT, async {
        let len = stream.read_u32().await.unwrap() as usize;
        assert!((1..=proto::MAX_TCP_FRAME).contains(&len));
        let chan = stream.read_u8().await.unwrap();
        let mut data = vec![0; len - 1];
        stream.read_exact(&mut data).await.unwrap();
        (chan, data)
    })
    .await
    .expect("TCP frame receive timed out")
}

async fn tcp_connect(relay: &RelayHandle, packet: &[u8]) -> TcpStream {
    let mut stream = TcpStream::connect(relay.tcp_local.unwrap()).await.unwrap();
    stream
        .write_all(&proto::tcp_frame(CHAN_CTRL, packet))
        .await
        .unwrap();
    stream
}

async fn tcp_bind(relay: &RelayHandle, packet: &[u8]) -> TcpStream {
    let mut stream = tcp_connect(relay, packet).await;
    let (chan, data) = tcp_frame(&mut stream).await;
    assert_eq!(chan, CHAN_CTRL);
    assert_eq!(proto::classify(&data), PacketClass::BindOk);
    stream
}

async fn assert_closed(stream: &mut TcpStream) {
    let mut byte = [0];
    match timeout(WAIT, stream.read(&mut byte)).await {
        Ok(Ok(0)) => {}
        Ok(Err(e))
            if matches!(
                e.kind(),
                io::ErrorKind::ConnectionReset
                    | io::ErrorKind::ConnectionAborted
                    | io::ErrorKind::BrokenPipe
            ) => {}
        result => panic!("obsolete/rejected TCP connection remained usable: {result:?}"),
    }
}

async fn assert_udp_quiet(socket: &UdpSocket) {
    let mut buf = [0; 2048];
    assert!(
        timeout(QUIET, socket.recv_from(&mut buf)).await.is_err(),
        "unexpected UDP delivery"
    );
}

#[tokio::test]
async fn nonloopback_admin_is_rejected_by_start_before_data_bind() {
    // Occupy the configured data port as an independent check that management validation runs first.
    let occupied = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    for addr in ["0.0.0.0:0", "192.0.2.1:0", "[::]:0", "[2001:db8::1]:0"] {
        let mut cfg = config();
        cfg.bind = occupied.local_addr().unwrap();
        cfg.admin_addr = Some(addr.parse().unwrap());
        let error = match relay::start(Arc::new(cfg)).await {
            Ok(handle) => {
                handle.stop().await;
                panic!("nonloopback admin accepted: {addr}");
            }
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("admin_addr"),
            "{addr}: {error:#}"
        );
    }
}

#[tokio::test]
async fn loopback_admin_still_serves_health() {
    let mut cfg = config();
    cfg.admin_addr = Some("127.0.0.1:0".parse().unwrap());
    let relay = relay::start(Arc::new(cfg)).await.unwrap();
    let mut stream = TcpStream::connect(relay.admin_local.unwrap())
        .await
        .unwrap();
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut response = String::new();
    timeout(WAIT, stream.read_to_string(&mut response))
        .await
        .unwrap()
        .unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    relay.stop().await;
}

#[tokio::test]
async fn saturated_nonce_cache_blocks_cross_transport_replay_and_fresh_bind() {
    let mut cfg = config();
    cfg.limits.nonce_capacity = 2;
    let relay = relay::start(Arc::new(cfg)).await.unwrap();
    let tag = [0x72; 16];
    let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let original = bind_packet(&tag, ROLE_INIT, 1);
    udp_bind(&udp, &relay, &original).await;
    let mut peer = tcp_bind(&relay, &bind_packet(&tag, ROLE_RESP, 2)).await;

    // Cache is exactly full: neither the oldest accepted Bind nor a fresh nonce may evict it.
    let mut replay = tcp_connect(&relay, &original).await;
    assert_closed(&mut replay).await;
    let mut fresh = tcp_connect(&relay, &bind_packet(&tag, ROLE_INIT, 3)).await;
    assert_closed(&mut fresh).await;
    let mut replay_again = tcp_connect(&relay, &original).await;
    assert_closed(&mut replay_again).await;

    udp.send_to(b"original UDP still owns slot", relay.udp_local)
        .await
        .unwrap();
    assert_eq!(
        tcp_frame(&mut peer).await,
        (CHAN_DATA, b"original UDP still owns slot".to_vec())
    );
    peer.write_all(&proto::tcp_frame(CHAN_DATA, b"reply"))
        .await
        .unwrap();
    assert_eq!(udp_recv(&udp, &relay).await, b"reply");
    relay.stop().await;
}

#[tokio::test]
async fn hot_udp_slot_rejects_tcp_takeover_and_unknown_udp_control() {
    let mut cfg = config();
    cfg.limits.rebind_guard = Duration::from_secs(30);
    let relay = relay::start(Arc::new(cfg)).await.unwrap();
    let tag = [0x73; 16];
    let original = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let intruder = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    udp_bind(&original, &relay, &bind_packet(&tag, ROLE_INIT, 1)).await;
    udp_bind(&peer, &relay, &bind_packet(&tag, ROLE_RESP, 2)).await;

    let mut tcp = tcp_connect(&relay, &bind_packet(&tag, ROLE_INIT, 3)).await;
    let (chan, data) = tcp_frame(&mut tcp).await;
    assert_eq!(chan, CHAN_CTRL);
    assert_eq!(data, proto::bind_err_packet(&tag, proto::ERR_TAG_FULL));
    assert_closed(&mut tcp).await;
    intruder
        .send_to(&bind_packet(&tag, ROLE_INIT, 4), relay.udp_local)
        .await
        .unwrap();
    assert_eq!(
        udp_recv(&intruder, &relay).await,
        proto::bind_err_packet(&tag, proto::ERR_TAG_FULL)
    );
    intruder
        .send_to(b"unauthorized data", relay.udp_local)
        .await
        .unwrap();
    intruder
        .send_to(&proto::bye_packet(&tag), relay.udp_local)
        .await
        .unwrap();
    assert_udp_quiet(&peer).await;

    peer.send_to(b"still original", relay.udp_local)
        .await
        .unwrap();
    assert_eq!(udp_recv(&original, &relay).await, b"still original");
    original
        .send_to(b"still paired", relay.udp_local)
        .await
        .unwrap();
    assert_eq!(udp_recv(&peer, &relay).await, b"still paired");
    assert_udp_quiet(&intruder).await;
    relay.stop().await;
}

#[tokio::test]
async fn replaced_tcp_closes_and_cannot_inject_or_remove_new_generation() {
    let relay = relay::start(Arc::new(config())).await.unwrap();
    let tag = [0x74; 16];
    let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    udp_bind(&peer, &relay, &bind_packet(&tag, ROLE_RESP, 1)).await;
    let mut old = tcp_bind(&relay, &bind_packet(&tag, ROLE_INIT, 2)).await;
    let mut current = tcp_bind(&relay, &bind_packet(&tag, ROLE_INIT, 3)).await;

    // Either OS rejection or a completed write is possible while the server closes the old socket.
    for frame in [
        proto::tcp_frame(CHAN_PING, b""),
        proto::tcp_frame(CHAN_DATA, b"stale injection"),
        proto::tcp_frame(CHAN_CTRL, &proto::bye_packet(&tag)),
    ] {
        let _ = old.write_all(&frame).await;
    }
    assert_closed(&mut old).await;
    assert_udp_quiet(&peer).await;
    current
        .write_all(&proto::tcp_frame(CHAN_DATA, b"current data"))
        .await
        .unwrap();
    assert_eq!(udp_recv(&peer, &relay).await, b"current data");
    peer.send_to(b"current reply", relay.udp_local)
        .await
        .unwrap();
    assert_eq!(
        tcp_frame(&mut current).await,
        (CHAN_DATA, b"current reply".to_vec())
    );
    relay.stop().await;
}

#[tokio::test]
async fn replaced_udp_source_cannot_send_data_or_bye_to_tcp_slot() {
    let relay = relay::start(Arc::new(config())).await.unwrap();
    let tag = [0x75; 16];
    let old = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    udp_bind(&old, &relay, &bind_packet(&tag, ROLE_INIT, 1)).await;
    udp_bind(&peer, &relay, &bind_packet(&tag, ROLE_RESP, 2)).await;
    let mut current = tcp_bind(&relay, &bind_packet(&tag, ROLE_INIT, 3)).await;
    old.send_to(b"obsolete UDP data", relay.udp_local)
        .await
        .unwrap();
    old.send_to(&proto::bye_packet(&tag), relay.udp_local)
        .await
        .unwrap();
    assert_udp_quiet(&peer).await;
    current
        .write_all(&proto::tcp_frame(CHAN_DATA, b"TCP owns slot"))
        .await
        .unwrap();
    assert_eq!(udp_recv(&peer, &relay).await, b"TCP owns slot");
    peer.send_to(b"TCP reply", relay.udp_local).await.unwrap();
    assert_eq!(
        tcp_frame(&mut current).await,
        (CHAN_DATA, b"TCP reply".to_vec())
    );
    assert_udp_quiet(&old).await;
    relay.stop().await;
}
