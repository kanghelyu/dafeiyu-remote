// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Dafeiyu Remote contributors
// Copyright (c) 2026 Dafeiyu Remote contributors
//! UDP 数据面：单 socket、单 recv 循环，不每包 spawn task。

use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Result;
use tokio::net::UdpSocket;
use tokio::sync::watch;
use tracing::{debug, info, trace};

use crate::config::Config;
use crate::metrics::{DropReason, Metrics};
use crate::proto::{self, PacketClass};
use crate::state::{self, BindOutcome, Egress, EgressKind, State};
use crate::unix_ms;

pub async fn run_udp(
    cfg: Arc<Config>,
    socket: Arc<UdpSocket>,
    state: Arc<Mutex<State>>,
    metrics: Arc<Metrics>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    // 65535 保证能收下任意 UDP 报文；缓冲区复用，零分配主路径。
    let mut buf = vec![0u8; 65535];
    loop {
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            recv = socket.recv_from(&mut buf) => {
                let (n, src) = match recv {
                    Ok(v) => v,
                    Err(e) => {
                        // Windows 上 ICMP 端口不可达会以 recv 错误形式出现；不能退出循环。
                        debug!(error = %e, "UDP recv_from 失败，继续");
                        continue;
                    }
                };
                metrics.add_in(n);
                let payload = &buf[..n];
                match proto::classify(payload) {
                    PacketClass::Bind => {
                        let outcome = {
                            let mut st = state::lock(&state);
                            st.bind(cfg.key.as_bytes(), payload, Some(src), None, Instant::now(), unix_ms())
                        };
                        handle_bind(&socket, src, outcome, &metrics, cfg.no_log_ips).await;
                    }
                    PacketClass::Bye => {
                        let byes = match proto::parse_bye_tag(payload) {
                            Some(tag) => {
                                let mut st = state::lock(&state);
                                st.unbind_addr(src, &tag)
                            }
                            None => Vec::new(),
                        };
                        for eg in byes {
                            send_egress(&socket, eg, &metrics).await;
                        }
                    }
                    PacketClass::Data => {
                        let eg = {
                            let mut st = state::lock(&state);
                            st.forward_udp(src, payload, Instant::now())
                        };
                        if let Some(eg) = eg {
                            send_egress(&socket, eg, &metrics).await;
                        }
                    }
                    // 客户端不应发送 BindOk/BindErr；头部像控制帧但长度非法也按坏包计。
                    PacketClass::BindOk | PacketClass::BindErr | PacketClass::Malformed => {
                        trace!("忽略非法控制报文，来自 {}", if cfg.no_log_ips { "<hidden>" } else { "peer" });
                        metrics.inc_drop(DropReason::BadPacket);
                    }
                }
            }
        }
    }
    info!("UDP 数据面已停止");
    Ok(())
}

async fn handle_bind(
    socket: &UdpSocket,
    src: std::net::SocketAddr,
    outcome: BindOutcome,
    metrics: &Metrics,
    no_log_ips: bool,
) {
    match outcome {
        BindOutcome::Ok {
            tag,
            role,
            reply,
            rebind,
            expires_in_s,
            ..
        } => {
            metrics.add_out(reply.len());
            if let Err(e) = socket.send_to(&reply, src).await {
                metrics.inc_drop(DropReason::ForwardError);
                debug!(error = %e, "发送 BindOk 失败");
            }
            let pref = proto::tag_prefix(&tag);
            if no_log_ips {
                info!(tag = %pref, role, rebind, expires_in_s, "bind 成功");
            } else {
                info!(tag = %pref, role, rebind, expires_in_s, peer = %src, "bind 成功");
            }
        }
        BindOutcome::Reject { reply, code } => {
            metrics.add_out(reply.len());
            if let Err(e) = socket.send_to(&reply, src).await {
                debug!(error = %e, "发送 BindErr 失败");
            }
            if no_log_ips {
                debug!(code, "bind 被拒绝（tag_full/busy）");
            } else {
                debug!(code, peer = %src, "bind 被拒绝（tag_full/busy）");
            }
        }
        BindOutcome::Silent => {}
    }
}

/// 执行一次转发：UDP 直接发；TCP 端点投递到其写任务。
pub async fn send_egress(socket: &UdpSocket, eg: Egress, metrics: &Metrics) {
    match eg.kind {
        EgressKind::Udp { to, data } => {
            let n = data.len();
            match socket.send_to(&data, to).await {
                Ok(_) => metrics.add_out(n),
                Err(e) => {
                    metrics.inc_drop(DropReason::ForwardError);
                    debug!(error = %e, "UDP 转发失败");
                }
            }
        }
        EgressKind::Tcp { tx, frame } => {
            let n = frame.len().saturating_sub(5);
            if tx.send(frame).await.is_ok() {
                metrics.add_out(n);
            } else {
                metrics.inc_drop(DropReason::ForwardError);
                debug!("TCP 端点写通道已关闭，丢弃转发");
            }
        }
    }
}
