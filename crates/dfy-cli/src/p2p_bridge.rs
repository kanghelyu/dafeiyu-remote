// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Dafeiyu Remote contributors

//! P2P 直连（STUN 候选收集 + 双端并发打洞）到 CLI 的桥。
//!
//! 业主决策 §17①：P2P 优先、中继兜底。v1 尚未接入信令服务，因此候选交换用
//! 「人工复制粘贴」：两端各自收集候选并打印，其中一端把对端候选通过
//! `--peer` 传入（或按提示从 stdin 粘贴）；两端在打洞窗口内并发向对方候选发包，
//! 成功后拿到已连通的 UDP socket，再交给 quinn（`agent_endpoint_on_socket` /
//! `connect_on_socket`），TLS/会话语义与直连完全一致。

use std::io::Write;
use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result};
use dfy_p2p::{gather_for_punch, punch, Candidate, NatHint, PunchOptions};

/// P2P 打洞参数（来自 CLI）。
#[derive(Debug, Clone)]
pub struct P2POptions {
    /// STUN 服务器列表（空 = 只收集本机 host 候选，仅限同一局域网）。
    pub stun: Vec<SocketAddr>,
    /// 对端候选（`--peer`；`None` = 从 stdin 读取）。
    pub peer: Vec<SocketAddr>,
    /// 本端 UDP 端口（0 = 内核分配；固定端口便于人工交换候选）。
    pub local_port: u16,
    /// 打洞窗口（默认 3 秒；局域网可更短）。
    pub punch_timeout: Duration,
}

impl Default for P2POptions {
    fn default() -> Self {
        Self {
            stun: Vec::new(),
            peer: Vec::new(),
            local_port: 0,
            punch_timeout: Duration::from_secs(3),
        }
    }
}

/// 打洞完成后的交接物：非阻塞 UDP socket + 对端真实地址。
pub struct P2PHandoff {
    /// 已与对端连通的 UDP socket（非阻塞），可直接交给 quinn。
    pub socket: std::net::UdpSocket,
    /// 对端候选地址（打洞确认的那个）。
    pub peer: SocketAddr,
    /// 本端实际绑定端口。
    pub local_port: u16,
    /// 本地候选（诊断/日志用）。
    pub candidates: Vec<Candidate>,
    /// NAT 映射一致性提示。
    pub nat_hint: NatHint,
}

/// 收集候选 → 打印 → 解析对端候选 → 并发打洞。
///
/// `role` 只用于日志前缀（"agent"/"view"）。
pub async fn prepare(role: &str, opts: &P2POptions) -> Result<P2PHandoff> {
    let gathered = gather_for_punch(&opts.stun, opts.local_port, Duration::from_secs(2))
        .await
        .context("收集 P2P 候选失败（STUN 不可达？）")?;
    println!("[{role}] 本端 UDP 端口 = {}", gathered.local_port);
    for c in &gathered.candidates {
        println!("[{role}] 候选 {:?} {}", c.kind, c.addr);
    }
    match gathered.nat_hint {
        NatHint::Unknown => println!("[{role}] NAT：未知（未配置 STUN 服务器）"),
        NatHint::ConsistentMapping => println!("[{role}] NAT：映射一致（可预期打洞成功）"),
        NatHint::InconsistentMapping => {
            println!("[{role}] NAT：映射不一致（疑似对称 NAT，打洞大概率失败，建议走中继）")
        }
    }

    let mut peers = opts.peer.clone();
    if peers.is_empty() {
        let prompt = format!(
            "[{role}] 请粘贴对端候选（逗号/空格分隔，回车确认；本端端口 {}）: ",
            gathered.local_port
        );
        let text = tokio::task::spawn_blocking(move || -> Result<String> {
            print!("{prompt}");
            std::io::stdout().flush().ok();
            let mut line = String::new();
            std::io::stdin()
                .read_line(&mut line)
                .context("读取对端候选失败")?;
            Ok(line)
        })
        .await
        .context("读取对端候选任务失败")??;
        peers = parse_candidates(&text)?;
    }
    anyhow::ensure!(
        !peers.is_empty(),
        "没有对端候选：请用 --peer 传入对端地址（或从 stdin 粘贴）"
    );

    let session = punch(
        gathered.socket,
        &peers,
        PunchOptions {
            deadline: opts.punch_timeout,
            ..PunchOptions::default()
        },
    )
    .await
    .context("P2P 打洞失败（对端未在窗口内响应；可改用 --relay）")?;
    println!(
        "[{role}] 打洞成功：本端 {} ↔ 对端 {}",
        session
            .socket
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| "?".into()),
        session.peer
    );
    Ok(P2PHandoff {
        socket: session.socket,
        peer: session.peer,
        local_port: gathered.local_port,
        candidates: gathered.candidates,
        nat_hint: gathered.nat_hint,
    })
}

/// 解析 `--peer` / stdin 输入：支持逗号、空格、分号分隔，IPv4/IPv6 均可。
pub fn parse_candidates(text: &str) -> Result<Vec<SocketAddr>> {
    let mut out = Vec::new();
    for part in text.split([',', ';', ' ', '\t', '\r', '\n']) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let addr: SocketAddr = part.parse().with_context(|| {
            format!("无法解析地址 {part}（示例：1.2.3.4:21120 或 [::1]:21120）")
        })?;
        out.push(addr);
    }
    Ok(out)
}

/// 解析 `--stun` 列表（逗号分隔的 host:port）。
pub fn parse_stun_list(text: &str) -> Result<Vec<SocketAddr>> {
    parse_candidates(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_separated_candidates() {
        let got = parse_candidates("127.0.0.1:21120, 10.0.0.2:21120; [::1]:21121").unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[1].to_string(), "10.0.0.2:21120");
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_candidates("not-an-addr").is_err());
        assert!(parse_candidates("").unwrap().is_empty());
    }
}
