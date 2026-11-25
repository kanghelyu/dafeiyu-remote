# 022 · 中继客户端模式：把 QUIC 端点架在 relay socket 上

状态：relay 服务端已实现（`dfy-relay`，29 单测 + selftest 8/8 + e2e 双端零丢包）；
客户端侧 `dfy-relay::bind_relay_socket` / `RelaySocket` 已由子代理实现并有 `tests/relay_quic.rs`；
CLI（`dfy-cli agent/view`）尚未接入 `--relay`。

## 1. 设计回顾：relay 是什么

`dfy-relay` 不是 TURN，也不是 QUIC 代理：

- 它**不解析 QUIC、不终结 TLS**，只按 16 字节 `tag` 把两端 UDP 包成对转发（零知识）。
- 每一端向 relay 发送 `BindRequest{tag, role, auth=HMAC-SHA256(relay_key, tag‖role‖nonce‖timestamp), nonce}`，
  收到 `BindAck{peer, expires_at, server_clock_ms}` 后，**对端地址 = relay 的 UDP 地址**（而非真实对端）。
- 之后两端都只与 relay 通信，relay 把数据包原样转发给 tag 的另一端；20s 保活、48h tag 生命周期、第三端点拒绝。
- TCP 21118 桥：UDP 被封锁时改走 `u32 len|u8 chan` 的 TLS/TCP 帧，语义相同（ALPN `dfyp-relay-tcp/1`）。

对 QUIC 来说，relay 就是一个"能同时对两个 NAT 后主机转发包的固定对端"——因此 relay 模式下**放弃打洞**，
直接让 quinn 连 relay 地址，把 UDP socket 换成 relay 的 socket（而不是新开 socket）。

## 2. 接入步骤（client / agent 对称）

```rust
use std::sync::Arc;
use dfy_relay::{bind_relay_socket, RelayClientConfig, Role};
use dfy_transport::{server_config / client_tls, SERVER_NAME};

let cfg = RelayClientConfig::with_relay_key(relay_addr, tag, Role::Client, &relay_key, nonce);
let relay = bind_relay_socket(&cfg).await?;          // 完成 Bind / BindAck / 启动保活
let peer = relay.peer_addr();                        // = relay 的 UDP 地址

// 用 relay 的 socket 直接构造 QUIC 端点（关键：不另开 socket，否则 relay 看到的是新地址）
let socket = relay.try_clone_socket()?;
let endpoint = quinn::Endpoint::new(
    quinn::EndpointConfig::default(),
    Some(server_cfg),                                // agent 端
    socket.try_into()?,                              // quinn::AsyncUdpSocket
    Arc::new(quinn::TokioRuntime),
)?;

let conn = endpoint.connect(peer, SERVER_NAME)?.await?;
// 之后完全复用现有握手/会话逻辑：server_handshake / client_handshake / serve_agent / run_client
```

要点：

1. **socket 复用**：`RelaySocket` 暴露 `try_clone_socket()`（同一 UDP socket 句柄的克隆）。
   quinn 收发包时，relay 视角的源地址始终是"客户端的真实 UDP 源"（若 NAT 重映射，relay 会更新会话端点并转发）。
2. **保活**：`RelaySocket` 内部每 20s 发 `KeepAlive`；`stop_keepalive()` 在会话结束时调用。
3. **角色/冲突**：同一 tag 只允许 agent+client 各一个；重复 `Bind` 会得到 `TAG_FULL`（一条连接被拒，不破坏已有会话）。
4. **失败回退**：`bind_relay_socket` 超时（默认 5s）或 `TAG_FULL` → 直接走 P2P/直连路径；
   反之 P2P 打洞失败 → 走 relay。两者共用同一份 QUIC 握手代码，只是 `peer` 地址与 socket 来源不同。

## 3. 与"可恢复会话迁移"的关系（业主决策 §17①）

v1 先实现"选择路径"（P2P 直连 or relay），**迁移**作为下一步：

- 劣化判定：`Heartbeat` RTT + QUIC 统计（丢包/重传/带宽估计）× 3 个采样（1.5s）超阈值。
- 迁移流程（`design/013-relay-and-p2p.md` §4）：`PrepareMigrate` → 新路径 0-RTT + TLS resumption →
  `Resume{session_token, last_seq}` → `ResumeOk` → 旧连接双发 2s → 关键帧 + seq 续传 + 修饰键快照。
- 关键：迁移的是 **QUIC 连接**（session token 绑定设备密钥），不是重开 TCP/UDP；
  因此 relay 侧只负责"把新 socket 的包按 tag 转发到对端"，不感知迁移语义——这正是零知识设计的好处。

## 4. 验收标准（CLI 接入后）

1. `dfy-cli agent --relay <addr> --relay-key <k> --tag <hex16>` 与 `view --relay ... --tag ...` 对跑，
   视频/心跳/输入全部正常，`view` 显示 `transport=relay`。
2. 断掉直连端口（防火墙阻断 UDP 直连）后自动/手动切 relay，画面在 1.5s 内恢复（<300ms 黑屏）。
3. relay 端 `metrics` 显示该 tag 的 `bytes_in/out`、`pkts_dropped`（限速）与 `peer_switch`。
4. 错误路径：错误 relay_key → `AUTH_FAILED`；过期 tag → `TAG_EXPIRED`；第三端点 → `TAG_FULL`。

## 5. 待办

- CLI：`agent`/`view` 增加 `--relay <addr> --relay-key <hex> --tag <hex>`，`real.rs` 按 §2 选 socket/peer。
- `run_client`/`serve_agent` 目前接收 `Connection`，无需改动——这是把 relay 接入成本压到最低的关键。
- 迁移协议（session_token、Resume/ResumeOk、双发窗口）在 `dfy-session` 里实现，relay 侧不需要改动。