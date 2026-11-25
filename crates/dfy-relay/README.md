# dfy-relay — DFYP/1 中继服务器

大肥鱼远程（Dafeiyu Remote）的 UDP/TCP 中继。中继**不解析 QUIC、不终结 TLS**，只维护
`tag → (端点A, 端点B)` 的配对关系并原样转发数据包；端到端加密由客户端负责，因此中继不可见
明文，也无法解密用户流量。

- 默认端口：**21117/udp** 数据面、**21118/tcp** 降级通道、**127.0.0.1:21119** 管理/指标（仅本机）。
- Linux 优先，Windows 可运行（`#[cfg(unix)]` / `#[cfg(windows)]` 分支：信号处理、配置文件权限）。
- 协议实现细节（信封字段表、与 `design/013-relay-and-p2p.md` 的差异、未实现项）见
  [DESIGN-NOTES.md](DESIGN-NOTES.md)。

## 编译

```bash
# 仓库根目录
cargo build --release -p dfy-relay
# 产物：target/release/dfy-relay
```

Windows（GNU 工具链）：

```powershell
rustup target list --installed        # stable-x86_64-pc-windows-gnu
cargo build --release -p dfy-relay
```

## 配置

完整示例（`/etc/dfy/relay.toml`，权限必须是 `0600`，因为含共享密钥）：

```toml
bind = "0.0.0.0:21117"          # UDP 数据面（客户端 relay.addr 填公网 IP:21117）
tcp_bind = "0.0.0.0:21118"      # TCP 降级通道；留空字符串 = 不启用
admin_addr = "127.0.0.1:21119"  # 本机管理/指标，禁止改成 0.0.0.0
relay_key = "base64:...."        # 32 字节共享密钥，必填；也可用 key_file = "/etc/dfy/relay.key"
max_sessions = 1024              # 并发 tag 会话上限
per_tag_mbps = 200               # 每 tag 转发限速；0 = 不限速
# max_global_mbps = 1000         # 全局转发限速（可选，注释掉 = 不限）
log = "info"                     # trace|debug|info|warn|error|off
log_json = false                 # true 时输出 JSON 日志（便于采集）
no_log_ips = true                # 默认 true：日志只打 tag 前缀，不打对端 IP

# 高级旋钮（一般不用改）
tag_ttl_hours = 48               # tag 生命周期上限
idle_timeout_s = 60              # 无流量回收（客户端 20s 保活，留足余量）
bind_window_s = 300              # bind 时间窗 ±300s
rebind_guard_ms = 2000           # 同 role 换地址：旧端点活跃期内的新地址判为第三端点拒绝
```

| 字段 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `bind` | string | `0.0.0.0:21117` | UDP 数据面监听地址 |
| `tcp_bind` | string? | `0.0.0.0:21118` | TCP 降级通道；空字符串关闭 |
| `admin_addr` | string | `127.0.0.1:21119` | 必须回环地址，否则 `config check` 报错 |
| `relay_key` | string | 无（必填） | `base64:` / `hex:` / 64 位裸 hex，解码后必须 32 字节 |
| `key_file` | path? | 无 | 从文件读 key（与 `relay_key` 二选一），内容同上格式或裸文本 |
| `max_sessions` | int | 1024 | 超过后新 bind 返回 `busy` |
| `per_tag_mbps` | float | 200 | 每 tag 令牌桶限速 |
| `max_global_mbps` | float? | 不限 | 全局令牌桶限速 |
| `log` / `log_json` | string/bool | `info`/false | `tracing-subscriber` 与 env-filter；`RUST_LOG` 优先 |
| `no_log_ips` | bool | true | true 时日志不含对端 IP（只打 tag hex 前 8 位） |
| `tag_ttl_hours` | float | 48 | 会话绝对寿命 |
| `idle_timeout_s` | int | 60 | 无流量回收时间 |
| `bind_window_s` | int | 300 | bind 时间戳窗口（抗重放） |
| `rebind_guard_ms` | int | 2000 | NAT 重绑定与第三端点的判定阈值 |

## 快速开始

```bash
# 1. 生成配置（随机 32B key，并直接打印客户端要填的字段）
sudo mkdir -p /etc/dfy
sudo dfy-relay config init --out /etc/dfy/relay.toml
# 或改造监听地址：--bind 0.0.0.0:21117 --tcp-bind 0.0.0.0:21118 --admin 127.0.0.1:21119

# 2. 校验（TOML / key 长度 / 地址 / admin 回环）
dfy-relay config check

# 3. 部署环境自检（端口占用/绑定权限/admin 可达）
dfy-relay doctor

# 4. 启动
dfy-relay serve

# 5. 进程内端到端自测（不联网、loopback 随机端口，成功输出 RELAY SELFTEST OK）
dfy-relay selftest

# 6. 本机运行状态
dfy-relay status            # GET /status
curl -s http://127.0.0.1:21119/healthz
curl -s http://127.0.0.1:21119/metrics
```

客户端（`dfy-cli` 或客户端 TOML）需要填同一份 `relay` 条目，`config init` 会直接打印：

```toml
[[relay]]
name = "relay-default"
addr = "203.0.113.10:21117"      # 中继公网地址 + UDP 端口
key = "base64:...."               # 与 relay_key 完全一致
tls = "auto"
priority = 10
enabled = true
```

## 客户端集成（QUIC over relay）

`dfy-relay` 不仅是可执行文件，也是一个库：客户端（Agent / Client）可以直接复用下面的
API 完成「中继 bind + 20s 保活」，再把同一个 UDP socket 交给 quinn。
这样 QUIC 的握手与 DATAGRAM 全部经过中继转发，而中继不解析 QUIC、也不终结 TLS。

### 配置字段

客户端 TOML 里对应的 `[[relay]]` 条目（`dfy-cli` 读取后映射为
[`RelayClientConfig`](https://docs.rs/dfy-relay)）：

| 字段 | 含义 | 备注 |
| --- | --- | --- |
| `addr` | 中继 UDP 地址，`公网IP:21117` | 服务端 `bind`；QUIC 的 `connect` 目标也是它 |
| `key` | 与中继 `relay_key` **完全一致** 的 32 字节共享密钥 | `base64:` / `hex:` / 裸 64 hex |
| `tag` | 16 字节配对 tag | 同一 tag + 不同 role 的两端被配成一对；随机生成，勿用可猜字符串 |
| `role` | `agent`（槽 0，建议长驻端先 bind）或 `client`（槽 1，拨号端后 bind） | 两端必须不同 |

> `tag` 不是凭据：能读出 bind 报文的人就能看到它。安全性来自 `key` 派生的
> HMAC 鉴权（±300s 时间窗 + nonce 防重放），所以 `key` 必须只发给受信客户端。

### 调用示例

```rust
use std::sync::Arc;
use std::time::Duration;

use dfy_relay::{bind_relay_socket, RelayClientConfig, Role};
use dfy_transport::{client_handshake, client_tls, connect, server_config, server_handshake, ServerAuth, TlsIdentity};

let cfg = RelayClientConfig {
    relay_addr: "203.0.113.10:21117".parse()?,
    tag: tag_bytes,                 // [u8; 16]
    role: Role::Client,
    key: key_bytes,                 // [u8; 32]，与中继 relay_key 一致
    nonce: rand::random(),
};

// 1) 中继 bind（内部会自动起 20s 保活任务）。必须比 QUIC 连接活得更久。
let relay = bind_relay_socket(&cfg).await?;

// 2) 用 socket 的 dup 副本构造 quinn endpoint（副本共享同一 UDP 端口）
let mut endpoint = quinn::Endpoint::new(
    quinn::EndpointConfig::default(),
    None,                                   // Client 不需要 server 配置
    relay.try_clone_socket()?,              // dup；原件留给 RelaySocket 保活
    Arc::new(quinn::TokioRuntime),
)?;
endpoint.set_default_client_config(client_tls(None)?.config.clone());

// 3) 目标是**中继地址**，server_name 用 "localhost"（自签证书 SAN 含 localhost）
let conn = endpoint.connect(relay.peer_addr(), "localhost")?.await?;
let (_welcome, channel) = client_handshake(&conn, password, "my-client", pubkey).await?;

// Agent 侧对称：server_config(&identity)? + endpoint.accept() + server_handshake(...)
// 两端都只需 bind 一次；中继按 (tag, role) 配对，两个槽位都 bind 后才开始转发。
```

`RelaySocket` 的其它公开能力：`socket()` / `try_clone_socket()`（交给 quinn 前先 dup）、
`peer_addr()`（连接目标）、`local_addr()`、`send_bind_request()`（手动保活，幂等）、
`expires_in_s()` / `server_clock_ms()`（来自 BindOk，可用于发现时钟偏差）、
`stop_keepalive()`（Drop 时自动中止）。

### 与 QUIC 的配合

- 两端各用自己的 relay socket 建 `quinn::Endpoint`：Agent 传 `Some(server_config)`，
  Client 传 `None` 并 `set_default_client_config`。
- 中继原样转发 payload，两端看到的 QUIC 对端地址都是**中继地址**；quinn 按
  connection ID 解复用，不需要真实对端地址（也因此两端都看不见对方的 IP）。
- **bind 顺序**不限，但两个 role 槽位都 bind 过、中继才会转发（未配对期间的数据包计
  `unpaired` 丢弃）。实战建议长驻 Agent 先 bind、拨号 Client 后 bind，重启 Agent
  不受拨号时序影响。
- 保活周期默认 20s，中继 `idle_timeout_s` 默认 60s。`RelaySocket` Drop 后中继在
  idle 超时后回收会话；重连需要重新 `bind_relay_socket`。
- 同一进程重复 bind 同一 `(tag, role)` 且源地址不变是幂等的（中继刷新 `last_seen`，
  不计 rebind），所以保活可以放心重发。

### 失败模式与回退

| 现象 | 原因 | 处理 |
| --- | --- | --- |
| `bind_relay_socket` 超时（默认 5s） | UDP 21117 未放行；或 `key` 不一致（中继鉴权失败**静默丢弃**）；或 tag 与该 role 已被占用 | 先核对 `key`（`config check` 不会显示它）；再查防火墙；再换 tag |
| 立即返回 `tag_full` | 该 tag 的此 role 槽位被另一地址占用，旧端点仍在 `rebind_guard`（默认 2s）窗口内 | 退避 2s 重试（NAT 换端口场景），或给这一对端换新 tag |
| 立即返回 `busy` | 中继 `max_sessions` 满 | 扩容或稍后重试 |
| QUIC 握手超时 | 两端 `role` 相同（互相顶替）；对端还没 bind；或中继路径单向不通 | 确认 role 不同且两端都已 bind；用 `selftest`/`status` 看会话数与 drop 计数 |
| 握手成功但 DATAGRAM 收不到 | 中继限速（默认每 tag 200Mbps）或 mtu/分片过小 | 提高 `per_tag_mbps` 或降低码率；`VideoReceiver::malformed()` 可区分解析失败 |

回退路径：中继的 TCP 降级通道（默认 `0.0.0.0:21118`）由上层按需选择，本节的
`RelaySocket` API 只覆盖 UDP 主通道；UDP 完全不可达时，退回直连或 TCP 通道。
集成测试见 `tests/relay_quic.rs`：同进程起 relay → 两个 role bind → QUIC 握手 →
10 帧 1KB DATAGRAM + Ping/Pong，并断言中继零 `bad_packet`/`unpaired` 丢弃。

## systemd 部署（Linux）

```bash
sudo install -m 0755 target/release/dfy-relay /usr/local/bin/dfy-relay
sudo useradd --system --no-create-home --shell /usr/sbin/nologin dfy-relay
sudo mkdir -p /etc/dfy
sudo dfy-relay config init --out /etc/dfy/relay.toml
sudo chown root:dfy-relay /etc/dfy/relay.toml && sudo chmod 0640 /etc/dfy/relay.toml
sudo install -m 0644 crates/dfy-relay/deploy/dfy-relay.service /etc/systemd/system/dfy-relay.service
sudo systemctl daemon-reload
sudo systemctl enable --now dfy-relay
sudo systemctl status dfy-relay
journalctl -u dfy-relay -f          # 日志（no_log_ips=true 不含对端 IP）
```

防火墙（`21117/udp` 必需，`21118/tcp` 仅当启用 TCP 降级；**不要**放行 21119）：

```bash
sudo ufw allow 21117/udp
sudo ufw allow 21118/tcp
# 云厂商安全组同样只需放行 UDP 21117（及可选 TCP 21118）
```

优雅关闭：`systemctl stop dfy-relay` 发送 SIGTERM → 停止收包 → 给剩余端点发 `Bye` → 退出 0。

## Docker

在**仓库根目录**构建（Dockerfile 需要 workspace 全部成员）：

```bash
docker build -f crates/dfy-relay/Dockerfile -t dfy-relay:0.1.0 .
docker run -d --name dfy-relay --restart unless-stopped \
  -p 21117:21117/udp -p 21118:21118/tcp \
  -v /etc/dfy/relay.toml:/etc/dfy/relay.toml:ro \
  dfy-relay:0.1.0 serve
# 管理面只监听容器内 127.0.0.1:21119，容器外不可达；需要查看时：
docker exec dfy-relay wget -qO- http://127.0.0.1:21119/healthz
```

> 容器内 `no_log_ips`、限速等行为与裸机完全一致；`relay_key` 必须通过只读挂载提供，不要写进镜像。

## 监控

`GET http://127.0.0.1:21119/metrics`（Prometheus 文本）：

| 指标 | 类型 | 含义 |
|---|---|---|
| `dfy_relay_sessions` | gauge | 当前活跃 tag 会话数（每个会话 2 端点） |
| `dfy_relay_bytes_total{dir="in"/"out"}` | counter | 收发字节数 |
| `dfy_relay_packets_total{dir="in"/"out"}` | counter | 收发报文数 |
| `dfy_relay_dropped_total{reason=...}` | counter | 丢弃计数，reason：`unknown_source`（未 bind/旧地址）、`auth`、`expired`、`replay`、`tag_full`、`busy`→`max_sessions`、`rate_limit`、`global_rate_limit`、`bad_packet`、`unpaired`、`forward_error` |
| `dfy_relay_rebinds_total` | counter | NAT 重绑定覆盖次数 |
| `dfy_relay_uptime_seconds` | gauge | 运行秒数 |

`GET /healthz` → `200 ok`（供 systemd/k8s 探活）。`GET /status` → JSON（`dfy-relay status` 使用）。

## 故障排查

| 现象 | 可能原因 / 处理 |
|---|---|
| 客户端 bind 超时、没有任何响应 | UDP 21117 未放行（ufw/安全组）；`bind` 写成了 127.0.0.1；`relay_key` 不一致（鉴权失败是**静默**丢弃，故意不暴露原因） |
| `tag_full` | 同一 tag 已有 2 个端点；若为 NAT 重绑定，需等 `rebind_guard_ms` 过后旧地址才算失效。检查客户端是否重复 bind |
| 反复 `auth` 丢弃计数增长 | 客户端 key 与服务器 `relay_key` 不一致，或客户端时钟偏差超过 ±300s（NTP） |
| 会话 60s 后消失 | 客户端没发 20s 保活；`idle_timeout_s` 放宽或修复客户端心跳 |
| `rate_limit` 增长 | 超过 `per_tag_mbps`；调大或设 0 |
| `dfy-relay status` 连接失败 | relay 未运行；或 `admin_addr` 不是 127.0.0.1 |
| Linux 端口绑定失败 | 21117/21118 >1024 不需 root；若改用 <1024 端口需 `CAP_NET_BIND_SERVICE` |
| Windows 上收不到包 | 防火墙入站规则放行 UDP 21117；`Ctrl+C` 优雅退出 |

自测与端到端脚本：

```bash
dfy-relay selftest                                   # 8 步：bind/转发/第三端点/鉴权/重绑定/限速/GC/TCP 桥接
python crates/dfy-relay/scripts/e2e.py \
  --relay 127.0.0.1:21117 --key "base64:..." --count 10   # 需 python 3.13+，仅标准库
```

## 安全边界

- 中继是**不可信转发点**：只看到 tag、包长、时间；载荷由客户端 E2E 加密。
- `relay_key` 是部署级共享密钥，必须 `0600`/只读挂载；泄露后所有 tag 可被伪造（换 key = 全量客户端一起换）。
- 管理面强制回环监听，`config check` 与 `doctor` 都会拦截非回环 `admin_addr`。
- `no_log_ips=true` 时日志不含对端地址，满足最小化日志要求。
- 每 tag 两端点上限、nonce 防重放（容量 8192、TTL 与 bind 窗口同量级）、±300s 时间窗、令牌桶限速，用于抵抗基本的滥用与放大攻击。