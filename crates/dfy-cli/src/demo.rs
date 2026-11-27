//! demo Agent / demo Client 的共享实现（`demo-agent`、`demo-client`、`selftest` 复用同一套代码）。
//!
//! 目的：验证 DFYP/1 的 **控制流握手 + 视频 DATAGRAM + input-move DATAGRAM + 心跳** 在
//! 真实 QUIC 连接上端到端可用，而不是实现产品级 Agent（无真实采集、无编码、无 FEC）。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, ensure, Context, Result};
use dfy_proto::consts::{
    BYE_USER, CHAN_INPUT_MOVE, CHAN_VIDEO, INPUT_KIND_MOUSE_MOVE, INPUT_STATE_DOWN,
};
use dfy_proto::envelope;
use dfy_proto::{decode_input_move, Envelope, InputEvent, Monitor, MonitorList};
use dfy_transport::{
    bind_agent_random, client_handshake, client_tls, connect, now_us, random_bytes,
    send_input_move, send_video_frame, server_handshake, AgentSession, ClientSession, ServerAuth,
    VideoReceiver, PING_TIMEOUT,
};
use quinn::{Connection, Endpoint};
// tokio 的 `Instant` 才能喂给 `tokio::time::sleep_until`；与 std 的同名类型用 `.into()` 转换。
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::credentials::SessionPassword;
use zeroize::Zeroizing;

/// demo 视频参数（与 `VideoConfig` 一致）。
pub const VIDEO_WIDTH: u32 = 1920;
pub const VIDEO_HEIGHT: u32 = 1080;
pub const VIDEO_FPS: u64 = 60;
pub const VIDEO_BITRATE_KBPS: u32 = 8000;
/// 每帧伪造负载大小（字节）；> 1100 会被拆成多个 DATAGRAM，正好验证分片路径。
pub const FRAME_PAYLOAD_BYTES: usize = 1200;
/// 每隔多少帧一个 keyframe。
pub const KEYFRAME_EVERY: u32 = 60;
/// 每秒鼠标移动 DATAGRAM 数。
pub const INPUT_MOVES_PER_SEC: u64 = 20;
/// Agent 上报的演示显示器。
const MONITOR_NAME: &str = "DFY-Virtual-1";

/// Agent 会话统计。
#[derive(Debug, Default, Clone)]
pub struct AgentStats {
    pub video_frames: u64,
    pub video_packets: u64,
    pub video_bytes: u64,
    pub input_packets: u64,
    pub control_in: u64,
    pub pings_answered: u64,
    pub rtt_ms: Option<f64>,
}

/// Client 会话统计。
#[derive(Debug, Default, Clone)]
pub struct ClientStats {
    pub video_frames: u64,
    pub video_packets: u64,
    pub video_bytes: u64,
    pub key_packets: u64,
    pub last_packets: u64,
    pub malformed_packets: u64,
    pub input_packets: u64,
    pub control_in: u64,
    pub pings_sent: u64,
    pub pongs: u64,
    pub rtt_ms: Option<f64>,
    /// `VideoConfig{width,height,fps,bitrate_kbps}`
    pub video_config: Option<(u32, u32, u32, u32)>,
    pub monitors: usize,
    pub permission_bitmap: u32,
}

/// Agent 主循环：接受连接，每个连接一个 task。
pub async fn serve(endpoint: Endpoint, password: SessionPassword) -> Result<()> {
    while let Some(incoming) = endpoint.accept().await {
        let password = password.clone();
        let remote = incoming.remote_address();
        tokio::spawn(async move {
            let connecting = match incoming.accept() {
                Ok(connecting) => connecting,
                Err(err) => {
                    warn!(%remote, error = %err, "接受 QUIC 连接失败");
                    return;
                }
            };
            let conn = match connecting.await {
                Ok(conn) => conn,
                Err(err) => {
                    warn!(%remote, error = %err, "QUIC 连接建立失败");
                    return;
                }
            };
            info!(%remote, "QUIC 连接已建立");
            match handle_agent_connection(conn, password).await {
                Ok(stats) => info!(
                    %remote,
                    frames = stats.video_frames,
                    packets = stats.video_packets,
                    input = stats.input_packets,
                    "会话正常结束"
                ),
                Err(err) => warn!(%remote, "会话失败: {err:#}"),
            }
        });
    }
    Ok(())
}

/// 单个 Agent 会话：握手 → 下发配置 → 推流循环。
async fn handle_agent_connection(
    conn: Connection,
    password: SessionPassword,
) -> Result<AgentStats> {
    let auth = ServerAuth::new(password.expose(), "dfy-demo-agent");
    let (welcome, channel) = server_handshake(&conn, &auth)
        .await
        .context("服务端握手失败")?;
    info!(
        session = %hex::encode(&welcome.session_id),
        client = %conn.remote_address(),
        "握手成功"
    );

    let mut session = AgentSession::new(conn, welcome, channel);
    // 握手后先下发初始配置（control 流，可靠有序）。
    session
        .send_ctrl(&Envelope::video_config(
            VIDEO_WIDTH,
            VIDEO_HEIGHT,
            VIDEO_FPS as u32,
            VIDEO_BITRATE_KBPS,
        ))
        .await
        .context("发送 VideoConfig 失败")?;
    session
        .send_ctrl(&Envelope {
            body: Some(envelope::Body::MonitorList(MonitorList {
                monitors: vec![Monitor {
                    id: 1,
                    width: VIDEO_WIDTH,
                    height: VIDEO_HEIGHT,
                    dpi: 96,
                    name: MONITOR_NAME.to_owned(),
                    primary: true,
                }],
            })),
        })
        .await
        .context("发送 MonitorList 失败")?;

    run_agent_loop(session).await
}

/// Agent 的「推流 + 心跳 + 收控制帧」循环。
async fn run_agent_loop(session: AgentSession) -> Result<AgentStats> {
    let AgentSession {
        conn,
        welcome: _,
        tx,
        rx,
        heartbeat,
    } = session;
    let mut tx = tx;
    let mut rx = rx;
    let mut hb = heartbeat;

    // 60 fps 视频 tick + 20/s 输入 tick；落后时丢弃积压 tick，不补发。
    let mut video_tick = tokio::time::interval(Duration::from_micros(1_000_000 / VIDEO_FPS));
    let mut input_tick = tokio::time::interval(Duration::from_millis(1000 / INPUT_MOVES_PER_SEC));
    video_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    input_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut stats = AgentStats::default();
    let mut payload = vec![0u8; FRAME_PAYLOAD_BYTES];
    let mut frame_id: u32 = 0;
    let mut frame_seq: u16 = 0;
    let mut stopped = false;

    while !stopped {
        tokio::select! {
            biased;

            // 1) 控制帧（Ping/Bye/其它）
            frame = rx.recv_ctrl() => {
                match frame {
                    Ok(Some(env)) => {
                        stats.control_in += 1;
                        let is_ping = matches!(&env.body, Some(envelope::Body::Ping(_)));
                        let bye_reason = match &env.body {
                            Some(envelope::Body::Bye(bye)) => Some(bye.reason),
                            _ => None,
                        };
                        if is_ping {
                            if let Some(pong) = hb.on_frame(&env) {
                                tx.send_ctrl(&pong).await.context("回复 Pong 失败")?;
                                stats.pings_answered += 1;
                            }
                        } else {
                            hb.on_frame(&env);
                        }
                        if let Some(reason) = bye_reason {
                            info!(reason, "收到客户端 Bye，结束会话");
                            stopped = true;
                        }
                    }
                    Ok(None) => {
                        info!("客户端关闭了 control 流");
                        stopped = true;
                    }
                    Err(err) => {
                        warn!("读取 control 帧失败: {err:#}");
                        stopped = true;
                    }
                }
            }

            // 2) 心跳到期：发 Ping；超时则断开
            _ = tokio::time::sleep_until(hb.deadline().into()) => {
                if hb.timed_out() {
                    warn!("心跳超时 {}s，断开连接", PING_TIMEOUT.as_secs());
                    break;
                }
                let ts = now_us();
                tx.send_ctrl(&Envelope::ping(ts))
                    .await
                    .context("发送 Ping 失败")?;
                hb.mark_ping_sent(ts);
            }

            // 3) 视频帧 tick
            _ = video_tick.tick() => {
                fill_pseudo_random(&mut payload, u64::from(frame_id) + 1);
                let keyframe = frame_id % KEYFRAME_EVERY == 0;
                match send_video_frame(&conn, frame_id, frame_seq, now_us() as u32, &payload, keyframe) {
                    Ok(sent) => {
                        stats.video_frames += 1;
                        stats.video_packets += sent.packets as u64;
                        stats.video_bytes += sent.bytes as u64;
                    }
                    Err(err) => {
                        debug!("发送视频 DATAGRAM 失败（连接可能已关闭）: {err:#}");
                        break;
                    }
                }
                frame_id = frame_id.wrapping_add(1);
                frame_seq = frame_seq.wrapping_add(1);
            }

            // 4) 鼠标移动 tick（不可靠 DATAGRAM）
            _ = input_tick.tick() => {
                let event = InputEvent {
                    kind: INPUT_KIND_MOUSE_MOVE,
                    hid_usage: 0,
                    mods: 0,
                    state: INPUT_STATE_DOWN,
                    x: u32::from(frame_seq) * 97 % 65536,
                    y: u32::from(frame_seq) * 31 % 65536,
                    dx: 1,
                    dy: 1,
                    unicode: 0,
                    ts_us: now_us(),
                };
                match send_input_move(&conn, &event) {
                    Ok(()) => stats.input_packets += 1,
                    Err(err) => {
                        debug!("发送 input-move DATAGRAM 失败: {err:#}");
                        break;
                    }
                }
            }
        }
    }

    stats.rtt_ms = hb.rtt_ms();
    let _ = tx.finish();
    Ok(stats)
}

/// Client 侧：连接 → 严格/宽松校验指纹 → 握手 → 收 `duration` 时长的数据 → Bye。
pub async fn run_client(
    addr: SocketAddr,
    password: &str,
    expect: Option<[u8; 32]>,
    duration: Duration,
    client_name: &str,
) -> Result<ClientStats> {
    let tls = client_tls(expect).context("构造客户端 TLS 配置失败")?;
    let (endpoint, conn) = connect(addr, tls.clone())
        .await
        .with_context(|| format!("连接 {addr} 失败（demo-agent 是否已启动？UDP 端口/防火墙？）"))?;

    match tls.observed_hex() {
        Some(observed) => {
            println!("[client] 服务端证书 SPKI-SHA256 = {observed}");
            if expect.is_some() {
                println!("[client] 指纹与 --expect-fingerprint 严格匹配 ✓");
            } else {
                println!("[client] 未指定 --expect-fingerprint：本次接受任意自签证书（TOFU 演示）");
            }
        }
        None => println!("[client] 未观察到服务端证书指纹（异常）"),
    }

    let (welcome, channel) = client_handshake(&conn, password, client_name, random_bytes(32))
        .await
        .context("客户端握手失败")?;
    println!(
        "[client] 握手成功：agent = {}, session = {}, 能力 = {} 项, 权限位图 = 0x{:08x}",
        welcome.agent_name,
        hex::encode(&welcome.session_id),
        welcome.capability.len(),
        welcome.permission_bitmap
    );

    let permission_bitmap = welcome.permission_bitmap;
    let mut stats =
        run_client_loop(ClientSession::new(conn.clone(), welcome, channel), duration).await?;
    stats.permission_bitmap = permission_bitmap;

    endpoint.close(0u32.into(), b"client done");
    Ok(stats)
}

/// Client 的「收 DATAGRAM + 收控制帧 + 心跳」循环。
async fn run_client_loop(session: ClientSession, duration: Duration) -> Result<ClientStats> {
    let ClientSession {
        conn,
        welcome: _,
        tx,
        rx,
        heartbeat,
    } = session;
    let mut tx = tx;
    let mut rx = rx;
    let mut hb = heartbeat;

    let deadline = Instant::now() + duration;
    let mut stats = ClientStats::default();
    let mut video = VideoReceiver::new();

    loop {
        if Instant::now() >= deadline {
            break;
        }
        tokio::select! {
            biased;

            // 1) control 帧
            frame = rx.recv_ctrl() => {
                match frame {
                    Ok(Some(env)) => {
                        stats.control_in += 1;
                        match &env.body {
                            Some(envelope::Body::Ping(_)) => {
                                if let Some(pong) = hb.on_frame(&env) {
                                    tx.send_ctrl(&pong).await.context("回复 Pong 失败")?;
                                }
                            }
                            Some(envelope::Body::VideoConfig(cfg)) => {
                                stats.video_config = Some((cfg.width, cfg.height, cfg.fps, cfg.bitrate_kbps));
                            }
                            Some(envelope::Body::MonitorList(list)) => {
                                stats.monitors = list.monitors.len();
                            }
                            Some(envelope::Body::Bye(bye)) => {
                                info!(reason = bye.reason, "Agent 发送 Bye");
                                break;
                            }
                            _ => {
                                hb.on_frame(&env);
                            }
                        }
                    }
                    Ok(None) => {
                        info!("Agent 关闭 control 流");
                        break;
                    }
                    Err(err) => {
                        warn!("读取 control 帧失败: {err:#}");
                        break;
                    }
                }
            }

            // 2) 收到的 DATAGRAM（视频 / 鼠标移动）
            datagram = conn.read_datagram() => {
                match datagram {
                    Ok(bytes) => match bytes.first().copied() {
                        Some(CHAN_VIDEO) => match video.ingest(&bytes) {
                            Ok(_) => {}
                            Err(err) => {
                                debug!("视频 DATAGRAM 解析失败: {err:#}");
                            }
                        },
                        Some(CHAN_INPUT_MOVE) => {
                            if let Ok(event) = decode_input_move(&bytes) {
                                let _ = event; // demo 只统计数量
                                stats.input_packets += 1;
                            }
                        }
                        _ => debug!("未知 DATAGRAM 通道"),
                    },
                    Err(err) => {
                        warn!("读 DATAGRAM 失败（连接已关闭）: {err}");
                        break;
                    }
                }
            }

            // 3) 心跳到期
            _ = tokio::time::sleep_until(hb.deadline().into()) => {
                if hb.timed_out() {
                    warn!("心跳超时 {}s，断开", PING_TIMEOUT.as_secs());
                    break;
                }
                let ts = now_us();
                tx.send_ctrl(&Envelope::ping(ts)).await.context("发送 Ping 失败")?;
                hb.mark_ping_sent(ts);
            }

            // 4) 到时退出
            _ = tokio::time::sleep_until(deadline) => break,
        }
    }

    stats.video_frames = video.frames() as u64;
    stats.video_packets = video.packets();
    stats.video_bytes = video.bytes();
    stats.key_packets = video.key_packets();
    stats.last_packets = video.last_packets();
    stats.malformed_packets = video.malformed();
    stats.pings_sent = hb.pings_sent;
    stats.pongs = hb.pongs_recv;
    stats.rtt_ms = hb.rtt_ms();

    // 正常收尾：发 Bye（可靠流）→ 半关闭 → 关闭连接。
    let _ = tx.send_ctrl(&Envelope::bye(BYE_USER)).await;
    let _ = tx.finish();
    tokio::time::sleep(Duration::from_millis(50)).await;
    conn.close(0u32.into(), b"client bye");
    Ok(stats)
}

const SELFTEST_PASSWORD: &str = "selftest-only-passphrase-2026";

/// 进程内端到端自测：随机端口起 Agent + Client，跑 1s，断言握手成功且视频 DATAGRAM ≥ 10。
pub async fn selftest() -> Result<()> {
    println!("=== dfy selftest：进程内 Agent + Client（127.0.0.1 随机端口）===");
    let (endpoint, identity, addr) = bind_agent_random().context("绑定 127.0.0.1:0 失败")?;
    println!("[selftest] Agent 监听 {addr}（UDP/QUIC，ALPN = dfyp/1）");
    println!(
        "[selftest] 证书 SPKI-SHA256 = {}",
        identity.fingerprint_hex()
    );

    let password = SessionPassword::new(Zeroizing::new(SELFTEST_PASSWORD.to_owned()))
        .expect("selftest passphrase");
    let agent_endpoint = endpoint.clone();
    let agent_task = tokio::spawn(async move {
        let incoming = agent_endpoint
            .accept()
            .await
            .context("endpoint 在连接到来前被关闭")?;
        let connecting = incoming
            .accept()
            .map_err(|err| anyhow!("接受 QUIC 连接失败: {err}"))?;
        let conn = connecting.await.context("QUIC 连接建立失败")?;
        handle_agent_connection(conn, password).await
    });

    let client_stats = run_client(
        addr,
        SELFTEST_PASSWORD,
        Some(identity.fingerprint),
        Duration::from_secs(1),
        "dfy-selftest-client",
    )
    .await
    .context("selftest 客户端失败")?;

    let agent_stats = match tokio::time::timeout(Duration::from_secs(5), agent_task).await {
        Ok(joined) => joined.context("Agent 任务 panic")??,
        Err(_) => {
            endpoint.close(0u32.into(), b"selftest timeout");
            anyhow::bail!("Agent 任务 5s 内未结束（可能 control 流未正确收尾）");
        }
    };

    endpoint.close(0u32.into(), b"selftest done");

    println!(
        "[selftest] client: 视频帧 = {}，DATAGRAM = {}（{}），key 包 = {}，LAST 包 = {}，input-move = {}，控制帧 = {}，RTT = {:?} ms",
        client_stats.video_frames,
        client_stats.video_packets,
        dfy_transport::human_bytes(client_stats.video_bytes),
        client_stats.key_packets,
        client_stats.last_packets,
        client_stats.input_packets,
        client_stats.control_in,
        client_stats.rtt_ms,
    );
    println!(
        "[selftest] agent : 视频帧 = {}，DATAGRAM = {}（{}），input-move = {}，pong = {}",
        agent_stats.video_frames,
        agent_stats.video_packets,
        dfy_transport::human_bytes(agent_stats.video_bytes),
        agent_stats.input_packets,
        agent_stats.pings_answered,
    );

    ensure!(
        client_stats.video_frames >= 1,
        "未收到任何完整视频帧（收到 DATAGRAM = {}）",
        client_stats.video_packets
    );
    ensure!(
        client_stats.video_packets >= 10,
        "视频 DATAGRAM 仅 {} 个，断言要求 ≥ 10",
        client_stats.video_packets
    );
    ensure!(
        client_stats.rtt_ms.is_some(),
        "未测到心跳 RTT（Ping/Pong 未完成）"
    );
    ensure!(
        agent_stats.pings_answered >= 1,
        "Agent 未回复任何 Pong（pings_answered = {}）",
        agent_stats.pings_answered
    );

    println!(
        "SELFTEST OK（握手成功，视频 DATAGRAM = {} ≥ 10，RTT = {:.3} ms）",
        client_stats.video_packets,
        client_stats.rtt_ms.unwrap_or_default()
    );
    Ok(())
}

/// 便宜的非加密伪随机填充（xorshift64），让每帧内容不同、可区分。
fn fill_pseudo_random(buf: &mut [u8], seed: u64) {
    let mut state = seed ^ 0x9E37_79B9_7F4A_7C15;
    if state == 0 {
        state = 0xDEAD_BEEF_CAFE_F00D;
    }
    for chunk in buf.chunks_mut(8) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let bytes = state.to_le_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
}

/// 便于测试：暴露 `Arc<Endpoint>` 的共享包装（`serve` 需要拥有 endpoint）。
#[allow(dead_code)]
pub fn shared_endpoint(endpoint: Endpoint) -> Arc<Endpoint> {
    Arc::new(endpoint)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pseudo_random_fills_and_varies() {
        let mut a = vec![0u8; 1200];
        let mut b = vec![0u8; 1200];
        fill_pseudo_random(&mut a, 1);
        fill_pseudo_random(&mut b, 2);
        assert_eq!(a.len(), 1200);
        assert_ne!(a, b, "不同种子应产生不同负载");
        assert_ne!(a, vec![0u8; 1200], "负载不应全零");
    }

    #[tokio::test]
    async fn selftest_passes() {
        selftest().await.expect("selftest 必须通过");
    }
}
