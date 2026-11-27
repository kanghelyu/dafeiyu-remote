// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Dafeiyu Remote contributors
// Copyright (c) 2026 Dafeiyu Remote contributors
//! 进程内端到端自测：真实启动中继（loopback 随机端口），用伪端点走完整协议。
//!
//! 覆盖：双端 bind / 双向转发 / 第三端点拒绝 / 错 key 与过期 ts 与重放静默 /
//! NAT 重绑定 / per-tag 限速 / 空闲回收 / TCP↔UDP 桥接。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, ensure, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::time::timeout;

use crate::auth::compute_auth;
use crate::config::{Config, Limits, LogLevel, RelayKey};
use crate::metrics::DropReason;
use crate::proto::*;
use crate::relay;
use crate::state::ROLE_INIT;
use crate::state::ROLE_RESP;
use crate::unix_ms;

const BIND_OK_WAIT: Duration = Duration::from_secs(2);
const SILENT_WAIT: Duration = Duration::from_millis(300);

fn random_tag() -> Tag {
    let mut tag = [0u8; TAG_LEN];
    for b in tag.iter_mut() {
        *b = rand::random::<u8>();
    }
    tag
}

fn test_config(key: RelayKey, idle: Duration) -> Config {
    // 小带宽便于在自测里稳定触发令牌桶丢弃。
    let limits = Limits {
        idle_timeout: idle,
        per_tag_bytes_per_s: Some(16_384.0),
        rebind_guard: Duration::from_millis(500),
        ..Limits::default()
    };
    Config {
        bind: "127.0.0.1:0".parse().expect("合法地址"),
        tcp_bind: Some("127.0.0.1:0".parse().expect("合法地址")),
        admin_addr: Some("127.0.0.1:0".parse().expect("合法地址")),
        key,
        log: LogLevel::Error,
        log_json: false,
        no_log_ips: true,
        limits,
    }
}

fn bind_packet_for(
    key: &RelayKey,
    tag: &Tag,
    role: u8,
    nonce_byte: u8,
    ts_offset_ms: i64,
) -> Vec<u8> {
    let ts = (unix_ms() as i64 + ts_offset_ms).max(0) as u64;
    let nonce = [nonce_byte; NONCE_LEN];
    let auth = compute_auth(key.as_bytes(), tag, role, &nonce, ts);
    crypt_bind(tag, role, &nonce, ts, &auth)
}

fn crypt_bind(tag: &Tag, role: u8, nonce: &Nonce, ts_ms: u64, auth: &Auth) -> Vec<u8> {
    crate::proto::bind_packet(tag, role, nonce, ts_ms, auth)
}

struct UdpClient {
    sock: UdpSocket,
    tag: Tag,
    role: u8,
}

impl UdpClient {
    async fn new(tag: Tag, role: u8) -> Result<Self> {
        let sock = UdpSocket::bind("127.0.0.1:0").await?;
        Ok(Self { sock, tag, role })
    }

    async fn send_bind(
        &self,
        relay: SocketAddr,
        key: &RelayKey,
        nonce_byte: u8,
        ts_offset_ms: i64,
    ) -> Result<Vec<u8>> {
        let pkt = bind_packet_for(key, &self.tag, self.role, nonce_byte, ts_offset_ms);
        self.sock.send_to(&pkt, relay).await?;
        Ok(pkt)
    }

    async fn send_raw(&self, relay: SocketAddr, data: &[u8]) -> Result<()> {
        self.sock.send_to(data, relay).await?;
        Ok(())
    }

    async fn recv(&self, dur: Duration) -> Result<Option<Vec<u8>>> {
        let mut buf = vec![0u8; 65535];
        match timeout(dur, self.sock.recv_from(&mut buf)).await {
            Ok(Ok((n, _))) => Ok(Some(buf[..n].to_vec())),
            Ok(Err(e)) => Err(e.into()),
            Err(_) => Ok(None),
        }
    }

    async fn expect_bind_ok(&self, dur: Duration) -> Result<u32> {
        let buf = self
            .recv(dur)
            .await?
            .ok_or_else(|| anyhow!("等待 BindOk 超时（tag={}）", tag_prefix(&self.tag)))?;
        ensure!(
            classify(&buf) == PacketClass::BindOk,
            "期望 BindOk，实际 kind 不匹配（{} 字节）",
            buf.len()
        );
        ensure!(buf.len() == BIND_OK_LEN, "BindOk 长度非法：{}", buf.len());
        ensure!(
            buf[HEADER_LEN..HEADER_LEN + TAG_LEN] == self.tag[..],
            "BindOk tag 与请求不匹配"
        );
        let expires = u32::from_be_bytes(
            buf[HEADER_LEN + TAG_LEN..HEADER_LEN + TAG_LEN + 4]
                .try_into()
                .expect("长度已校验"),
        );
        Ok(expires)
    }

    async fn expect_no_reply(&self, dur: Duration) -> Result<()> {
        let got = self.recv(dur).await?;
        ensure!(
            got.is_none(),
            "本应静默丢弃，却收到 {} 字节响应",
            got.map(|b| b.len()).unwrap_or(0)
        );
        Ok(())
    }
}

/// TCP 侧伪端点：连上后发 chan=2 的 Bind 帧，等 BindOk 帧。
async fn tcp_connect(
    relay: SocketAddr,
    key: &RelayKey,
    tag: &Tag,
    role: u8,
    nonce_byte: u8,
) -> Result<TcpStream> {
    let mut stream = TcpStream::connect(relay).await?;
    let pkt = bind_packet_for(key, tag, role, nonce_byte, 0);
    stream
        .write_all(&tcp_frame(CHAN_CTRL, &pkt))
        .await
        .context("写入 TCP 握手帧失败")?;
    let (chan, payload) = tcp_read_frame(&mut stream, BIND_OK_WAIT)
        .await?
        .ok_or_else(|| anyhow!("TCP 握手无响应"))?;
    ensure!(chan == CHAN_CTRL, "TCP 握手应答信道非法：{chan}");
    ensure!(
        classify(&payload) == PacketClass::BindOk,
        "TCP 握手应答不是 BindOk"
    );
    Ok(stream)
}

async fn tcp_read_frame(stream: &mut TcpStream, dur: Duration) -> Result<Option<(u8, Vec<u8>)>> {
    let fut = async {
        let mut lenbuf = [0u8; 4];
        if stream.read_exact(&mut lenbuf).await.is_err() {
            return Ok::<Option<(u8, Vec<u8>)>, std::io::Error>(None);
        }
        let len = u32::from_be_bytes(lenbuf) as usize;
        if len == 0 || len > MAX_TCP_FRAME {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "TCP 帧长度非法",
            ));
        }
        let mut chan = [0u8; 1];
        stream.read_exact(&mut chan).await?;
        let mut payload = vec![0u8; len - 1];
        if len > 1 {
            stream.read_exact(&mut payload).await?;
        }
        Ok(Some((chan[0], payload)))
    };
    match timeout(dur, fut).await {
        Ok(r) => Ok(r?),
        Err(_) => Ok(None),
    }
}

fn payload_1k() -> Vec<u8> {
    (0..1024u32).map(|i| (i % 251) as u8).collect()
}

/// 执行全部自测步骤；成功返回 `Ok(())`，调用方打印 `RELAY SELFTEST OK`。
pub async fn run() -> Result<()> {
    let key = RelayKey::random();
    let relay = relay::start(Arc::new(test_config(key.clone(), Duration::from_secs(60))))
        .await
        .context("启动自测中继失败")?;
    println!(
        "  中继已就绪：UDP={} TCP={:?} ADMIN={:?}",
        relay.udp_local, relay.tcp_local, relay.admin_local
    );
    let addr = relay.udp_local;

    // [1/8] 双端 bind
    let tag = random_tag();
    let a = UdpClient::new(tag, ROLE_INIT).await?;
    let b = UdpClient::new(tag, ROLE_RESP).await?;
    let _bind_a = a.send_bind(addr, &key, 1, 0).await?;
    let expires_a = a.expect_bind_ok(BIND_OK_WAIT).await.context("role0 bind")?;
    let bind_b = b.send_bind(addr, &key, 2, 0).await?;
    let expires_b = b.expect_bind_ok(BIND_OK_WAIT).await.context("role1 bind")?;
    ensure!(expires_a > 0 && expires_b > 0, "BindOk 有效期必须为正");
    ensure!(
        crate::state::lock(&relay.state).session_count() == 1,
        "两个端点应合成 1 个会话"
    );
    println!("  [1/8] 双端 bind（expires={expires_a}s）: OK");

    // [2/8] 双向 1KB 转发（包体不变）
    let one_k = payload_1k();
    a.send_raw(addr, &one_k).await?;
    let back = b
        .recv(BIND_OK_WAIT)
        .await?
        .ok_or_else(|| anyhow!("role1 未收到转发包"))?;
    ensure!(back == one_k, "A→B 转发包体不一致");
    b.send_raw(addr, &one_k).await?;
    let back = a
        .recv(BIND_OK_WAIT)
        .await?
        .ok_or_else(|| anyhow!("role0 未收到转发包"))?;
    ensure!(back == one_k, "B→A 转发包体不一致");
    println!("  [2/8] 1KB 双向转发（包体一致）: OK");

    // [3/8] 第三端点被拒（tag_full）
    let c = UdpClient::new(tag, ROLE_INIT).await?;
    c.send_bind(addr, &key, 3, 0).await?;
    let resp = c
        .recv(BIND_OK_WAIT)
        .await?
        .ok_or_else(|| anyhow!("第三端点未收到拒绝应答"))?;
    ensure!(
        classify(&resp) == PacketClass::BindErr,
        "第三端点应收到 BindErr"
    );
    ensure!(resp.len() == BIND_ERR_LEN, "BindErr 长度非法");
    let code = resp[BIND_ERR_LEN - 1];
    ensure!(code == ERR_TAG_FULL, "拒绝码应为 tag_full，实际 {code}");
    println!("  [3/8] 第三端点被拒（code=tag_full）: OK");

    // [4/8] 错 key / 过期 ts / 重放：全部静默
    let sessions_before = crate::state::lock(&relay.state).session_count();
    let tag_bad = random_tag();
    let w = UdpClient::new(tag_bad, ROLE_INIT).await?;
    let wrong_key = RelayKey::random();
    w.send_bind(addr, &wrong_key, 11, 0).await?;
    w.expect_no_reply(SILENT_WAIT)
        .await
        .context("错 key 应静默")?;
    w.send_bind(addr, &key, 12, -3_600_000).await?;
    w.expect_no_reply(SILENT_WAIT)
        .await
        .context("过期时间戳应静默")?;
    b.send_raw(addr, &bind_b).await?;
    b.expect_no_reply(SILENT_WAIT)
        .await
        .context("重放 bind 应静默")?;
    ensure!(
        crate::state::lock(&relay.state).session_count() == sessions_before,
        "被拒的 bind 不应创建会话"
    );
    println!("  [4/8] 错 key / 过期 ts / 重放：全部静默丢弃 OK");

    // [5/8] NAT 重绑定：同一 tag/role、新地址覆盖旧地址
    tokio::time::sleep(Duration::from_millis(650)).await;
    let a2 = UdpClient::new(tag, ROLE_INIT).await?;
    a2.send_bind(addr, &key, 4, 0).await?;
    a2.expect_bind_ok(BIND_OK_WAIT)
        .await
        .context("重绑定 bind")?;
    a2.send_raw(addr, b"after-rebind").await?;
    let got = b
        .recv(BIND_OK_WAIT)
        .await?
        .ok_or_else(|| anyhow!("重绑定后新地址未转发"))?;
    ensure!(got == b"after-rebind", "重绑定后包体不一致");
    a.send_raw(addr, b"stale-address").await?;
    b.expect_no_reply(Duration::from_millis(250))
        .await
        .context("旧地址在新地址接管后不应再转发")?;
    println!("  [5/8] NAT 重绑定覆盖旧地址（旧地址失效）: OK");

    // [6/8] per-tag 令牌桶限速
    let tag_rate = random_tag();
    let r1 = UdpClient::new(tag_rate, ROLE_INIT).await?;
    let r2 = UdpClient::new(tag_rate, ROLE_RESP).await?;
    r1.send_bind(addr, &key, 31, 0).await?;
    r1.expect_bind_ok(BIND_OK_WAIT).await?;
    r2.send_bind(addr, &key, 32, 0).await?;
    r2.expect_bind_ok(BIND_OK_WAIT).await?;
    let drop_before = relay.metrics.dropped(DropReason::RateLimit);
    let burst = [0u8; 1024];
    for chunk in 0..13 {
        for _ in 0..16 {
            r1.send_raw(addr, &burst).await?;
        }
        if chunk < 12 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut delivered = 0usize;
    while r2.recv(Duration::from_millis(50)).await?.is_some() {
        delivered += 1;
    }
    let dropped = relay.metrics.dropped(DropReason::RateLimit) - drop_before;
    ensure!(dropped > 0, "令牌桶未触发限速丢弃（delivered={delivered}）");
    ensure!(delivered < 200, "限速后转发包数不应达到 200");
    println!("  [6/8] per-tag 限速：丢弃 {dropped} 包、转发 {delivered} 包 OK");

    // [7/8] 空闲回收：第二个中继（idle=200ms）在 GC tick 后清会话并发 Bye
    let relay2 = relay::start(Arc::new(test_config(
        key.clone(),
        Duration::from_millis(200),
    )))
    .await
    .context("启动 GC 自测中继失败")?;
    let tag_gc = random_tag();
    let g1 = UdpClient::new(tag_gc, ROLE_INIT).await?;
    let g2 = UdpClient::new(tag_gc, ROLE_RESP).await?;
    g1.send_bind(relay2.udp_local, &key, 41, 0).await?;
    g1.expect_bind_ok(BIND_OK_WAIT).await?;
    g2.send_bind(relay2.udp_local, &key, 42, 0).await?;
    g2.expect_bind_ok(BIND_OK_WAIT).await?;
    ensure!(
        crate::state::lock(&relay2.state).session_count() == 1,
        "GC 自测会话未建立"
    );
    tokio::time::sleep(Duration::from_millis(1500)).await;
    ensure!(
        crate::state::lock(&relay2.state).session_count() == 0,
        "空闲会话未被 GC 回收"
    );
    let bye1 = g1.recv(BIND_OK_WAIT).await?;
    let bye2 = g2.recv(BIND_OK_WAIT).await?;
    ensure!(
        bye1.as_deref().map(classify) == Some(PacketClass::Bye)
            && bye2.as_deref().map(classify) == Some(PacketClass::Bye),
        "GC 删除会话后应向端点发送 Bye"
    );
    relay2.stop().await;
    println!("  [7/8] 空闲回收 + Bye 通知: OK");

    // [8/8] TCP↔UDP 桥接：TCP role0 + UDP role1，chan0 双向透传
    let tcp_addr = relay
        .tcp_local
        .ok_or_else(|| anyhow!("自测中继未启用 TCP"))?;
    let tag_tcp = random_tag();
    let mut tcp = tcp_connect(tcp_addr, &key, &tag_tcp, ROLE_INIT, 51).await?;
    let u2 = UdpClient::new(tag_tcp, ROLE_RESP).await?;
    u2.send_bind(addr, &key, 52, 0).await?;
    u2.expect_bind_ok(BIND_OK_WAIT).await?;
    tcp.write_all(&tcp_frame(CHAN_DATA, b"tcp-to-udp")).await?;
    let got = u2
        .recv(BIND_OK_WAIT)
        .await?
        .ok_or_else(|| anyhow!("UDP 端点未收到 TCP 发来的数据"))?;
    ensure!(got == b"tcp-to-udp", "TCP→UDP 包体不一致");
    u2.send_raw(addr, b"udp-to-tcp").await?;
    let (chan, payload) = tcp_read_frame(&mut tcp, BIND_OK_WAIT)
        .await?
        .ok_or_else(|| anyhow!("TCP 端点未收到 UDP 发来的数据"))?;
    ensure!(
        chan == CHAN_DATA && payload == b"udp-to-tcp",
        "UDP→TCP 帧非法"
    );
    println!("  [8/8] TCP↔UDP 桥接（chan0 双向）: OK");

    relay.stop().await;
    println!();
    println!("RELAY SELFTEST OK");
    Ok(())
}
