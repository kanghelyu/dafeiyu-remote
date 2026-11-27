// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Dafeiyu Remote contributors
// Copyright (c) 2026 Dafeiyu Remote contributors
//! 服务装配：绑定端口、启动数据处理任务、GC、优雅关闭。

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::info;

use crate::admin;
use crate::config::Config;
use crate::metrics::Metrics;
use crate::state::{self, State};
use crate::tcp;
use crate::udp;

pub struct RelayHandle {
    pub udp_local: SocketAddr,
    pub tcp_local: Option<SocketAddr>,
    pub admin_local: Option<SocketAddr>,
    pub metrics: Arc<Metrics>,
    pub state: Arc<Mutex<State>>,
    udp: Arc<UdpSocket>,
    shutdown: watch::Sender<bool>,
    joins: Vec<JoinHandle<()>>,
}

impl RelayHandle {
    pub fn request_stop(&self) {
        let _ = self.shutdown.send(true);
    }

    /// 优雅关闭：停 accept/recv → 给剩余端点发 Bye → 指标落定。
    pub async fn stop(mut self) {
        let _ = self.shutdown.send(true);
        for join in self.joins.drain(..) {
            let _ = join.await;
        }
        let byes = {
            let mut st = state::lock(&self.state);
            st.drain_all()
        };
        for eg in byes {
            udp::send_egress(&self.udp, eg, &self.metrics).await;
        }
        self.metrics.set_sessions(0);
        info!("中继已优雅关闭");
    }
}

/// 启动中继（UDP 数据面必需；TCP/管理面可选）。
pub async fn start(cfg: Arc<Config>) -> Result<RelayHandle> {
    // Config 字段可直接构造，不能只依赖文件加载或 config check 的校验。
    cfg.validate_admin_addr()?;
    let udp_sock =
        Arc::new(UdpSocket::bind(cfg.bind).await.with_context(|| {
            format!("UDP 数据面绑定失败：{}（端口被占用或权限不足）", cfg.bind)
        })?);
    let udp_local = udp_sock.local_addr()?;
    let metrics = Arc::new(Metrics::new());
    let state = Arc::new(Mutex::new(State::new(cfg.limits.clone(), metrics.clone())));
    let (tx, rx) = watch::channel(false);
    let mut joins: Vec<JoinHandle<()>> = Vec::new();

    let udp_task = udp::run_udp(
        cfg.clone(),
        udp_sock.clone(),
        state.clone(),
        metrics.clone(),
        rx.clone(),
    );
    joins.push(tokio::spawn(async move {
        if let Err(e) = udp_task.await {
            tracing::error!(error = %e, "UDP 数据面任务异常退出");
        }
    }));

    let tcp_local = match cfg.tcp_bind {
        Some(addr) => {
            let listener = TcpListener::bind(addr)
                .await
                .with_context(|| format!("TCP 降级通道绑定失败：{addr}"))?;
            let local = listener.local_addr()?;
            let tcp_task = tcp::run_tcp(
                cfg.clone(),
                listener,
                state.clone(),
                udp_sock.clone(),
                metrics.clone(),
                rx.clone(),
            );
            joins.push(tokio::spawn(async move {
                if let Err(e) = tcp_task.await {
                    tracing::error!(error = %e, "TCP 降级通道任务异常退出");
                }
            }));
            Some(local)
        }
        None => None,
    };

    let admin_local = match cfg.admin_addr {
        Some(addr) => {
            let listener = TcpListener::bind(addr)
                .await
                .with_context(|| format!("admin 绑定失败：{addr}"))?;
            let local = listener.local_addr()?;
            let admin_task = admin::run_admin(
                cfg.clone(),
                listener,
                state.clone(),
                metrics.clone(),
                rx.clone(),
            );
            joins.push(tokio::spawn(async move {
                if let Err(e) = admin_task.await {
                    tracing::error!(error = %e, "admin/指标任务异常退出");
                }
            }));
            Some(local)
        }
        None => None,
    };

    joins.push(tokio::spawn(gc_loop(
        udp_sock.clone(),
        state.clone(),
        metrics.clone(),
        rx.clone(),
    )));

    info!(
        udp = %udp_local,
        tcp = ?tcp_local,
        admin = ?admin_local,
        max_sessions = cfg.limits.max_sessions,
        "dfy-relay 已启动"
    );

    Ok(RelayHandle {
        udp_local,
        tcp_local,
        admin_local,
        metrics,
        state,
        udp: udp_sock,
        shutdown: tx,
        joins,
    })
}

/// 垃圾回收：1s tick 清理空闲 60s / 超 48h 的 tag，并给端点发 Bye。
async fn gc_loop(
    udp: Arc<UdpSocket>,
    state: Arc<Mutex<State>>,
    metrics: Arc<Metrics>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            _ = tick.tick() => {
                let byes = {
                    let mut st = state::lock(&state);
                    st.gc(Instant::now())
                };
                for eg in byes {
                    udp::send_egress(&udp, eg, &metrics).await;
                }
            }
        }
    }
}
