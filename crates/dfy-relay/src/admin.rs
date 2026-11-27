// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Dafeiyu Remote contributors
// Copyright (c) 2026 Dafeiyu Remote contributors
//! 本机管理面：手写 HTTP/1.1（无 axum/hyper），仅监听回环地址。
//!
//! - `GET /healthz` → `200 ok`
//! - `GET /metrics` → Prometheus 文本
//! - `GET /status`  → JSON（`dfy-relay status` 使用）

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::time::timeout;
use tracing::{debug, info};

use crate::config::Config;
use crate::metrics::Metrics;
use crate::state::{self, State};
use crate::unix_ms;

pub async fn run_admin(
    cfg: Arc<Config>,
    listener: TcpListener,
    state: Arc<Mutex<State>>,
    metrics: Arc<Metrics>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let addr = listener.local_addr()?;
    info!(%addr, "admin/指标服务已启动（仅本机）");
    loop {
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            accepted = listener.accept() => {
                if let Ok((stream, _peer)) = accepted {
                    let cfg2 = cfg.clone();
                    let state2 = state.clone();
                    let metrics2 = metrics.clone();
                    tokio::spawn(async move {
                        let _ = handle(stream, cfg2, state2, metrics2).await;
                    });
                }
            }
        }
    }
    Ok(())
}

async fn handle(
    mut stream: TcpStream,
    cfg: Arc<Config>,
    state: Arc<Mutex<State>>,
    metrics: Arc<Metrics>,
) -> Result<()> {
    let mut buf = [0u8; 4096];
    let n = match timeout(Duration::from_secs(2), stream.read(&mut buf)).await {
        Ok(Ok(n)) if n > 0 => n,
        _ => return Ok(()),
    };
    let req = String::from_utf8_lossy(&buf[..n]);
    let first = req.lines().next().unwrap_or_default();
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let path = parts.next().unwrap_or_default();
    let path = path.split('?').next().unwrap_or_default();

    let (status, content_type, body) = if method != "GET" {
        (
            "405 Method Not Allowed",
            "text/plain; charset=utf-8",
            "method not allowed\n".to_string(),
        )
    } else {
        match path {
            "/healthz" => ("200 OK", "text/plain; charset=utf-8", "ok\n".to_string()),
            "/metrics" => (
                "200 OK",
                "text/plain; version=0.0.4; charset=utf-8",
                metrics.render_prometheus(),
            ),
            "/status" => (
                "200 OK",
                "application/json; charset=utf-8",
                status_json(&cfg, &state, &metrics),
            ),
            _ => (
                "404 Not Found",
                "text/plain; charset=utf-8",
                "not found\n".to_string(),
            ),
        }
    };
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.flush().await;
    Ok(())
}

fn status_json(cfg: &Config, state: &Mutex<State>, metrics: &Metrics) -> String {
    let sessions = state::lock(state).session_count();
    let tcp_bind = match cfg.tcp_bind {
        Some(a) => format!("\"{a}\""),
        None => "null".to_string(),
    };
    let admin = match cfg.admin_addr {
        Some(a) => format!("\"{a}\""),
        None => "null".to_string(),
    };
    format!(
        concat!(
            "{{",
            "\"ok\":true,",
            "\"version\":\"{}\",",
            "\"uptime_seconds\":{},",
            "\"time_ms\":{},",
            "\"sessions\":{},",
            "\"max_sessions\":{},",
            "\"bytes_in\":{},",
            "\"bytes_out\":{},",
            "\"dropped_total\":{},",
            "\"rebinds_total\":{},",
            "\"bind\":\"{}\",",
            "\"tcp_bind\":{},",
            "\"admin\":{},",
            "\"no_log_ips\":{}",
            "}}\n"
        ),
        env!("CARGO_PKG_VERSION"),
        metrics.uptime_seconds(),
        unix_ms(),
        sessions,
        cfg.limits.max_sessions,
        metrics.bytes_in(),
        metrics.bytes_out(),
        metrics.dropped_sum(),
        metrics.rebinds(),
        cfg.bind,
        tcp_bind,
        admin,
        cfg.no_log_ips,
    )
}

/// `dfy-relay status` 用：请求 /status 并返回响应正文。
pub async fn fetch_status(addr: SocketAddr) -> Result<String> {
    let mut stream = timeout(Duration::from_secs(2), TcpStream::connect(addr))
        .await
        .map_err(|_| anyhow::anyhow!("连接 admin {addr} 超时"))??;
    let haddr = if addr.is_ipv6() {
        format!("[{addr}]")
    } else {
        addr.to_string()
    };
    let req = format!("GET /status HTTP/1.1\r\nHost: {haddr}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await?;
    let mut resp = Vec::new();
    timeout(Duration::from_secs(3), stream.read_to_end(&mut resp))
        .await
        .map_err(|_| anyhow::anyhow!("读取 admin 响应超时"))??;
    let text = String::from_utf8_lossy(&resp);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("admin 响应格式非法"))?;
    if !head.starts_with("HTTP/1.1 200") {
        let line = head.lines().next().unwrap_or_default();
        return Err(anyhow::anyhow!("admin 返回非 200：{line}"));
    }
    debug!("admin 状态查询成功");
    Ok(body.to_string())
}
