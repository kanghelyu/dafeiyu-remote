// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 回环集成测试：用一个最小 STUN 服务器 + 两个本地 UDP socket 验证
//! `stun_probe` / `stun_probe_with_change` / `gather_for_punch` / `punch` 的端到端行为。
//!
//! 这些测试不依赖公网，也无法覆盖真实 NAT 行为（见 `design/023-p2p-punch.md` §8）：
//! - 覆盖层：STUN 消息编解码、事务 ID 匹配、超时重传、CHANGE-REQUEST 传递、
//!   XOR-MAPPED-ADDRESS 解析、候选收集后同一 socket 复用、双端并发打洞与数据互通；
//! - 未覆盖层：真实公网 STUN、NAT 映射/过滤行为、对称 NAT、跨网卡与 IPv6、
//!   MESSAGE-INTEGRITY 认证。

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dfy_p2p::{
    gather_for_punch, punch, stun_probe, stun_probe_with_change, ChangeFlags, PunchOptions,
    StunMessage,
};
use tokio::net::UdpSocket;

/// 最小 STUN 服务器观察到的请求记录（由测试断言重传 / CHANGE-REQUEST）。
#[derive(Debug, Default)]
struct ServerState {
    requests: u32,
    sources: Vec<SocketAddr>,
    change_requests: Vec<u32>,
}

/// 启动最小 STUN 服务器：解析 Binding Request，回 XOR-MAPPED-ADDRESS = 请求源地址。
///
/// - `drop_first`：前 N 个请求丢弃（模拟丢包，用于验证超时重传）；
/// - `respond_from_secondary`：从另一个 socket/端口回包（模拟 CHANGE-REQUEST 换端口）。
async fn spawn_stun_server(
    drop_first: u32,
    respond_from_secondary: bool,
) -> (SocketAddr, SocketAddr, Arc<Mutex<ServerState>>) {
    let main = UdpSocket::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .expect("绑定主 STUN socket");
    let secondary = UdpSocket::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .expect("绑定备用 STUN socket");
    let main_addr = main.local_addr().expect("读取主 STUN 地址");
    let secondary_addr = secondary.local_addr().expect("读取备用 STUN 地址");
    let state = Arc::new(Mutex::new(ServerState::default()));
    let shared = state.clone();

    tokio::spawn(async move {
        let mut buffer = vec![0u8; 1500];
        loop {
            let Ok((len, source)) = main.recv_from(&mut buffer).await else {
                return;
            };
            let Ok(request) = StunMessage::decode(&buffer[..len]) else {
                continue;
            };
            if !request.is_binding_request() {
                continue;
            }

            // 记录观察值后再决定是否丢包（丢包也要计数，才能断言重传次数）。
            let should_reply = {
                let mut guard = shared.lock().expect("STUN 状态锁未中毒");
                guard.requests += 1;
                guard.sources.push(source);
                if let Some(bits) = request.change_request() {
                    guard.change_requests.push(bits);
                }
                guard.requests > drop_first
            };
            if !should_reply {
                continue;
            }

            let (responder, other) = if respond_from_secondary {
                (&secondary, main_addr)
            } else {
                (&main, secondary_addr)
            };
            let response =
                StunMessage::binding_success(request.transaction_id, source, Some(other));
            if let Err(error) = responder.send_to(&response.encode(), source).await {
                eprintln!("STUN 服务器回包失败：{error}");
                return;
            }
        }
    });

    (main_addr, secondary_addr, state)
}

/// 排空打洞残留帧，避免影响后续业务数据的断言。
async fn drain_residual_frames(socket: &UdpSocket, idle: Duration) {
    let mut buffer = [0u8; 1500];
    loop {
        match tokio::time::timeout(idle, socket.recv_from(&mut buffer)).await {
            Ok(Ok(_)) => continue,
            _ => return,
        }
    }
}

#[tokio::test]
async fn stun_probe_reports_mapped_and_other_address() {
    let (server, other, state) = spawn_stun_server(0, false).await;

    let started = Instant::now();
    let result = stun_probe(server, Duration::from_secs(2))
        .await
        .expect("回环 STUN 探测应成功");
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(2),
        "探测耗时 {elapsed:?} 应小于 2s"
    );
    assert!(result.rtt > Duration::ZERO && result.rtt <= elapsed);
    assert_eq!(result.mapped.ip(), Ipv4Addr::LOCALHOST);
    assert_eq!(result.other, Some(other), "应解析出 OTHER-ADDRESS");
    assert_eq!(result.source, server, "回包应来自探测的服务器地址");

    // XOR-MAPPED-ADDRESS 必须等于服务器看到的源地址（含端口）。
    let observed = state.lock().expect("状态锁未中毒").sources[0];
    assert_eq!(result.mapped, observed);
    assert_eq!(result.mapped.port(), observed.port());
}

#[tokio::test]
async fn stun_probe_retransmits_after_packet_loss() {
    // 前 2 个请求丢弃，第 3 个才回：只有重传才能成功。
    let (server, _other, state) = spawn_stun_server(2, false).await;
    let result = stun_probe(server, Duration::from_secs(1))
        .await
        .expect("丢包后应靠重传成功");
    assert_eq!(result.mapped.ip(), Ipv4Addr::LOCALHOST);
    let requests = state.lock().expect("状态锁未中毒").requests;
    assert!(requests >= 3, "应至少发送 3 次请求，实际 {requests}");
}

#[tokio::test]
async fn stun_probe_times_out_when_server_is_silent() {
    // 绑定但不回包：模拟 STUN 服务器不可达/被防火墙吞掉。
    let silent = UdpSocket::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .expect("绑定静默 socket");
    let silent_addr = silent.local_addr().expect("读取静默地址");

    let started = Instant::now();
    let error = stun_probe(silent_addr, Duration::from_millis(300))
        .await
        .expect_err("服务器静默时必须超时报错");
    let elapsed = started.elapsed();
    assert!(
        error.to_string().contains("超时"),
        "错误信息应说明超时：{error}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "应在 deadline 附近结束：{elapsed:?}"
    );
}

#[tokio::test]
async fn stun_probe_with_change_request_reaches_server_and_changes_source() {
    let (server, secondary, state) = spawn_stun_server(0, true).await;

    let result = stun_probe_with_change(server, Duration::from_secs(2), ChangeFlags::IP_AND_PORT)
        .await
        .expect("带 CHANGE-REQUEST 的探测应成功");

    // 服务器侧必须看到 CHANGE-IP(0x04) | CHANGE-PORT(0x02)。
    let bits = {
        let guard = state.lock().expect("状态锁未中毒");
        guard.change_requests.clone()
    };
    assert_eq!(
        bits.first().copied(),
        Some(0x06),
        "CHANGE-REQUEST 位应为 0x06：{bits:?}"
    );
    // 回包来自备用 socket（换端口），并且映射地址仍是客户端真实地址。
    assert_eq!(result.source, secondary);
    assert_eq!(result.mapped.ip(), Ipv4Addr::LOCALHOST);
    let observed = state.lock().expect("状态锁未中毒").sources[0];
    assert_eq!(result.mapped, observed);
}

#[tokio::test]
async fn gathered_sockets_punch_and_exchange_data() {
    // 两侧都走真实的候选收集路径（无 STUN 服务器 → 只有 Host 候选/无候选），
    // 然后复用 gather 返回的同一个 socket 打洞——这是生产路径的关键约束。
    let gathered_a = gather_for_punch(&[], 0, Duration::from_millis(100))
        .await
        .expect("A 侧收集候选");
    let gathered_b = gather_for_punch(&[], 0, Duration::from_millis(100))
        .await
        .expect("B 侧收集候选");
    let port_a = gathered_a.local_port;
    let port_b = gathered_b.local_port;
    assert!(port_a > 0 && port_b > 0 && port_a != port_b);
    assert_eq!(gathered_a.nat_hint, dfy_p2p::NatHint::Unknown);

    let addr_a = SocketAddr::from((Ipv4Addr::LOCALHOST, port_a));
    let addr_b = SocketAddr::from((Ipv4Addr::LOCALHOST, port_b));
    let opts = PunchOptions::default()
        .with_deadline(Duration::from_secs(1))
        .with_interval(Duration::from_millis(20));

    let candidates_a = [addr_b];
    let candidates_b = [addr_a];
    let (session_a, session_b) = tokio::join!(
        punch(gathered_a.socket, &candidates_a, opts),
        punch(gathered_b.socket, &candidates_b, opts),
    );
    let session_a = session_a.expect("A 侧打洞应成功");
    let session_b = session_b.expect("B 侧打洞应成功");
    assert_eq!(session_a.peer, addr_b);
    assert_eq!(session_b.peer, addr_a);

    // 打洞成功后 socket 交回上层（此处转成 tokio 仅为方便测试收发）。
    let socket_a = UdpSocket::from_std(session_a.into_parts().0).expect("A 转 tokio socket");
    let socket_b = UdpSocket::from_std(session_b.into_parts().0).expect("B 转 tokio socket");
    drain_residual_frames(&socket_a, Duration::from_millis(50)).await;
    drain_residual_frames(&socket_b, Duration::from_millis(50)).await;

    socket_a
        .send_to(b"hello-b", addr_b)
        .await
        .expect("A 发送业务数据");
    let mut buffer = [0u8; 128];
    let (len, from) = tokio::time::timeout(Duration::from_secs(1), socket_b.recv_from(&mut buffer))
        .await
        .expect("B 应收到业务数据")
        .expect("B 接收失败");
    assert_eq!(&buffer[..len], b"hello-b");
    assert_eq!(from, addr_a);

    socket_b
        .send_to(b"hello-a", addr_a)
        .await
        .expect("B 回发业务数据");
    let (len, from) = tokio::time::timeout(Duration::from_secs(1), socket_a.recv_from(&mut buffer))
        .await
        .expect("A 应收到回包")
        .expect("A 接收失败");
    assert_eq!(&buffer[..len], b"hello-a");
    assert_eq!(from, addr_b);
}

#[tokio::test]
async fn punch_reports_error_for_unreachable_candidates() {
    // 没有对端监听：Windows 上可能收到 ICMP 端口不可达（ConnectionReset），
    // 打洞实现必须把这类错误当瞬时错误忽略，最终以超时结束而不是提前失败。
    let gathered = gather_for_punch(&[], 0, Duration::from_millis(100))
        .await
        .expect("收集候选");
    let unused = UdpSocket::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .expect("绑定一个不存在的对端");
    let unused_addr = unused.local_addr().expect("读取地址");
    drop(unused); // 关闭端口，制造不可达

    let opts = PunchOptions::default()
        .with_deadline(Duration::from_millis(400))
        .with_interval(Duration::from_millis(30));
    let error = punch(gathered.socket, &[unused_addr], opts)
        .await
        .expect_err("对端不可达时应超时失败");
    assert!(error.to_string().contains("打洞超时"), "错误信息：{error}");
}
