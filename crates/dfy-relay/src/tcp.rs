// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Dafeiyu Remote contributors
// Copyright (c) 2026 Dafeiyu Remote contributors
//! TCP 降级通道：`u32 len | u8 chan | payload` 帧，与 UDP 路径同语义、
//! 由会话表按 tag 桥接（TCP↔UDP / TCP↔TCP）。
//!
//! 首帧必须为 `chan=2`（控制）且 payload 为 Bind 报文，握手 2s 超时。
//! TODO(TLS): 目前为 TCP 明文最小版；TLS 终结需 rustls，见 DESIGN-NOTES.md。

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{mpsc, watch, Semaphore};
use tokio::time::timeout;
use tracing::{debug, info};

use crate::config::Config;
use crate::metrics::{DropReason, Metrics};
use crate::proto::{self, CHAN_CTRL, CHAN_DATA, CHAN_PING};
use crate::state::{self, BindOutcome, EndpointSource, State};
use crate::udp::send_egress;
use crate::unix_ms;

pub async fn run_tcp(
    cfg: Arc<Config>,
    listener: TcpListener,
    state: Arc<Mutex<State>>,
    udp: Arc<UdpSocket>,
    metrics: Arc<Metrics>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    // 未鉴权连接也占资源：并发连接数上限 = max_sessions*2（至少 32）。
    let limit = cfg.limits.max_sessions.saturating_mul(2).max(32);
    let sem = Arc::new(Semaphore::new(limit));
    loop {
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, peer)) => {
                        let permit = match sem.clone().try_acquire_owned() {
                            Ok(p) => p,
                            Err(_) => {
                                debug!("TCP 并发连接数达上限，拒绝新连接");
                                drop(stream);
                                continue;
                            }
                        };
                        let cfg2 = cfg.clone();
                        let state2 = state.clone();
                        let udp2 = udp.clone();
                        let metrics2 = metrics.clone();
                        tokio::spawn(async move {
                            // permit 在连接结束时释放。
                            let _permit = permit;
                            handle_conn(cfg2, stream, peer, state2, udp2, metrics2).await;
                        });
                    }
                    Err(e) => debug!(error = %e, "TCP accept 失败"),
                }
            }
        }
    }
    info!("TCP 降级通道已停止");
    Ok(())
}

async fn handle_conn(
    cfg: Arc<Config>,
    stream: TcpStream,
    peer: SocketAddr,
    state: Arc<Mutex<State>>,
    udp: Arc<UdpSocket>,
    metrics: Arc<Metrics>,
) {
    let _ = stream.set_nodelay(true);
    let (mut rd, wr) = stream.into_split();
    let handshake = timeout(Duration::from_secs(2), read_frame(&mut rd)).await;
    let (chan, payload) = match handshake {
        Ok(Ok(Some(f))) => f,
        _ => return,
    };
    if chan != CHAN_CTRL {
        debug!(peer = %peer, "TCP 首帧不是控制信道，关闭");
        return;
    }

    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(256);
    let outcome = {
        let mut st = state::lock(&state);
        st.bind(
            cfg.key.as_bytes(),
            &payload,
            None,
            Some(tx.clone()),
            Instant::now(),
            unix_ms(),
        )
    };
    let mut wr = wr;
    let (tag, role, id, mut closed, reply) = match outcome {
        BindOutcome::Ok {
            tag,
            role,
            id,
            closed,
            reply,
            rebind,
            ..
        } => {
            debug!(tag = %proto::tag_prefix(&tag), role, rebind, "TCP bind 成功");
            (tag, role, id, closed, reply)
        }
        BindOutcome::Reject { reply, code } => {
            metrics.add_out(reply.len());
            let frame = proto::tcp_frame(CHAN_CTRL, &reply);
            let _ = timeout(Duration::from_secs(2), wr.write_all(&frame)).await;
            debug!(code, "TCP bind 被拒绝");
            return;
        }
        BindOutcome::Silent => return,
    };
    let source = EndpointSource::Tcp { id };
    drop(tx);
    let frame = proto::tcp_frame(CHAN_CTRL, &reply);
    let wrote_ack = tokio::select! {
        biased;
        _ = closed.changed() => false,
        written = timeout(Duration::from_secs(2), wr.write_all(&frame)) => {
            matches!(written, Ok(Ok(())))
        }
    };
    if !wrote_ack {
        let byes = state::lock(&state).unbind(&tag, role, source);
        for eg in byes {
            send_egress(&udp, eg, &metrics).await;
        }
        return;
    }
    metrics.add_out(reply.len());

    // 写任务：所有发往此 TCP 端点的帧都经 mpsc 串行化写入。
    let mut writer = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            if wr.write_all(&frame).await.is_err() {
                break;
            }
        }
        let _ = wr.shutdown().await;
    });

    loop {
        let incoming = tokio::select! {
            biased;
            // 当前 endpoint 被替换或回收后，旧 reader/writer 都必须结束。
            _ = closed.changed() => break,
            _ = &mut writer => break,
            frame = read_frame(&mut rd) => frame,
        };
        match incoming {
            Ok(Some((chan, data))) => {
                metrics.add_in(data.len());
                match chan {
                    CHAN_DATA => {
                        let eg = {
                            let mut st = state::lock(&state);
                            st.forward(tag, role, source, &data, Instant::now())
                        };
                        if let Some(eg) = eg {
                            tokio::select! {
                                biased;
                                _ = closed.changed() => break,
                                _ = send_egress(&udp, eg, &metrics) => {}
                            }
                        }
                    }
                    CHAN_PING => {
                        if !state::lock(&state).touch(&tag, role, source, Instant::now()) {
                            break;
                        }
                    }
                    CHAN_CTRL => {
                        // 客户端 Bye：只接受本 tag，清理时仍检查 endpoint 身份。
                        if let Some(t) = proto::parse_bye_tag(&data) {
                            if t == tag {
                                break;
                            }
                        }
                    }
                    other => debug!(chan = other, "未知 TCP 信道，忽略"),
                }
            }
            Ok(None) => break,
            Err(e) => {
                debug!(error = %e, "TCP 帧读取失败，关闭连接");
                break;
            }
        }
    }

    // 先结束旧 writer，避免等待对端 Bye 投递时仍持有旧连接。
    writer.abort();
    let byes = {
        let mut st = state::lock(&state);
        st.unbind(&tag, role, source)
    };
    for eg in byes {
        send_egress(&udp, eg, &metrics).await;
    }
    metrics.inc_drop(DropReason::ForwardError);
}

/// 读一个 TCP 帧：`u32 len | u8 chan | payload(len-1)`，EOF 返回 `Ok(None)`。
async fn read_frame<R: AsyncRead + Unpin>(rd: &mut R) -> std::io::Result<Option<(u8, Vec<u8>)>> {
    let mut lenbuf = [0u8; 4];
    match rd.read_exact(&mut lenbuf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(lenbuf) as usize;
    if len == 0 || len > proto::MAX_TCP_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "TCP 帧长度非法",
        ));
    }
    let mut chan = [0u8; 1];
    rd.read_exact(&mut chan).await?;
    let mut payload = vec![0u8; len - 1];
    if len > 1 {
        rd.read_exact(&mut payload).await?;
    }
    Ok(Some((chan[0], payload)))
}
