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

## 5. CLI 接入（v0.1 已完成）

- `dfy-cli agent|view` 新增：`--relay <ADDR>`、`--relay-key <KEY>`（`hex:…`/`base64:…`/裸 hex）、
  `--relay-tag <HEX>`（可选）。只给 tag/key 不给 addr 会直接报错。
- 默认 tag 派生：`tag = BLAKE3(password ‖ "\0dfy-relay-tag\0" ‖ relay_addr)[:16]`。
  两端口令相同即可自动配对；需要显式控制时用 `--relay-tag`。
- `crates/dfy-cli/src/relay_bridge.rs`：`RelayOptions{addr,tag,key}` + `bind(role)`；
- `crates/dfy-transport/src/endpoint.rs`：`agent_endpoint_on_socket(socket, identity)` 与
  `connect_on_socket(socket, tls, peer)`，把 `RelaySocket::try_clone_socket()` 交还给 quinn。
  `serve_agent`/`run_client` 一行未改——这是接入成本最低的关键。
- `real.rs` 中继分支：agent 先 `relay.bind(Role::Agent)`、生成 TLS 身份、用中继 socket 建端点；
  view 先 `relay.bind(Role::Client)`、`connect_on_socket(…, relay.addr)`。中继 socket 句柄持有到函数结束，
  以便保活任务存活。

## 6. 实现与验证（已完成，2026-11-25）

实测环境：本机回环，relay `0.0.0.0:21117`（TCP 降级 21118、admin 127.0.0.1:21119）、tag `fa423018cf1e7bbec2862f45f3dd07d2`。

命令：

```powershell
# 1) 中继
dfy-relay serve --config .tmp/relay.toml
# 2) 被控端（经中继）
dfy agent --port 21116 --relay 127.0.0.1:21117 --relay-key 'base64:…' --fps 30 --bitrate 4000
# 3) 主控端（经中继，开启剪贴板）
dfy view --relay 127.0.0.1:21117 --relay-key 'base64:…' --seconds 5 --clipboard
```

结果：

| 指标 | 直连基线 | 经中继 |
| --- | --- | --- |
| 握手 | 本机 socket 直连 | `peer=127.0.0.1:21117`（中继地址），TLS 1.3 握手成功 |
| 视频 DATAGRAM | 398 / 6s | **359 / 6s** |
| 重组帧 | 60 | 60 |
| 关键帧 / 解析失败 | 1 / 0 | 1 / 0 |
| 心跳 RTT | 0.571 ms | 0.41–0.51 ms（agent 侧 29.55 ms 为会话结束快照） |
| 输入注入 | 16/16 | 本用例未注入（`--no-input` 对照） |
| 剪贴板 | 20 B 文本闭环 | **24.55 KiB 分块闭环**（1 清单 / 1 拉取 / 1 分块 / 1 确认） |

结论：**中继模式端到端可用**——视频、心跳、剪贴板全部经中继转发，两端代码路径与直连完全一致
（只是 socket 来源与 `peer` 地址不同）。这验证了"零知识 tag 配对 UDP 转发"的设计假设：
中继不解析 QUIC，TLS 仍端到端，客户端只看到中继地址。

## 7. 下一步

- P2P 打洞（STUN/ICE-lite）与"直连优先、relay 兜底"的选择逻辑（业主决策 §17①）。
- 路径劣化判定与会话迁移（PrepareMigrate/Resume/ResumeOk，`design/013` §4）。
- relay 端 `transport=relay` 显示、metrics 里的 `peer_switch` 计数、错误路径集成测试
  （错误 key/过期 tag/第三端点）。
- 多中继池与健康探测（客户端 profile 列表）。
