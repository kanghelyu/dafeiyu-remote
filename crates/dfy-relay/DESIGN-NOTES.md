# dfy-relay 实现说明（DESIGN-NOTES）

本文件记录 v1 实现里**自定的协议细节**、**与 `design/013-relay-and-p2p.md` 的差异**、以及
**未实现项**。设计文档是定稿输入，本 crate 只做实现；两者冲突时以本文件标注的"实现选择"为准。

## 1. 信封协议 DFYP/1（clean-room 自定）

### 1.1 公共控制头（仅控制报文，6 字节）

| 偏移 | 长度 | 字段 | 取值 |
|---|---|---|---|
| 0 | 4 | `magic` | ASCII `"DFYR"` |
| 4 | 1 | `version` | `1` |
| 5 | 1 | `kind` | 1=Bind、2=BindOk、3=BindErr、4=Bye |

`kind` 之后的字段按 kind 解释。**数据包不带任何中继头**：客户端 bind 成功后，中继按源地址查表，
把收到的整包（含客户端自己的 QUIC/UDP 载荷）原样转发给对端，不修改任何字节。
判断"是不是控制报文"只看前 6 字节是否匹配 magic+version+kind；不匹配即视为数据。
误判概率上界 1/2^48（magic 4B + version 1B + kind 取值域），对 UDP 载荷可忽略。

### 1.2 Bind（客户端 → 中继，`kind=1`，总长 79 字节）

| 偏移 | 长度 | 字段 | 说明 |
|---|---|---|---|
| 0 | 6 | 控制头 | magic/version/kind=1 |
| 6 | 16 | `tag` | 会话标识（随机 16B，两端共享） |
| 22 | 1 | `role` | 0=发起方、1=应答方（每个 tag 每 role 一个槽位） |
| 23 | 16 | `nonce` | 每次 bind 随机，防重放 |
| 39 | 8 | `ts_ms` | 客户端 Unix 毫秒时间戳，大端 |
| 47 | 32 | `auth` | HMAC-SHA256 输出 |

鉴权输入（无分隔、定长拼接，顺序固定）：

```
auth = HMAC-SHA256(relay_key, tag[16] || role[1] || nonce[16] || ts_ms_be[8])
```

- `relay_key`：32 字节部署级共享密钥（`base64:`/`hex:`/64 位 hex 配置）。
- 时间窗：`|now_ms - ts_ms| <= bind_window_s * 1000`（默认 ±300s）。
- 重放防护：`(tag, role, nonce)` 三元组在窗口内去重（哈希集容量默认 8192，容量满/TTL 到期淘汰）。
- 校验顺序：长度 → role 合法 → 时间窗 → HMAC（常量时间比较）→ nonce 去重 → 会话/槽位逻辑。
  任一步失败一律**静默丢弃**，不回任何错误（不向攻击者暴露失败原因），只增加
  `dfy_relay_dropped_total{reason=...}` 计数（`auth`/`expired`/`replay`/`bad_packet` 等）。

### 1.3 BindOk（中继 → 客户端，`kind=2`，34 字节）

| 偏移 | 长度 | 字段 |
|---|---|---|
| 6 | 16 | `tag`（回显请求 tag，客户端必须校验） |
| 22 | 4 | `expires_in_s`（会话剩余寿命秒，按 `tag_ttl_hours` 计算，clamp 到 ≥1） |
| 26 | 8 | `server_ms`（服务端时间，便于客户端发现时钟偏差） |

### 1.4 BindErr（中继 → 客户端，`kind=3`，23 字节）

| 偏移 | 长度 | 字段 |
|---|---|---|
| 6 | 16 | `tag` |
| 22 | 1 | `code`：1=`tag_full`（该 tag 已有 2 个端点）、2=`busy`（`max_sessions` 已满） |

保留 `busy`：设计文档只要求 `tag_full`；`max_sessions` 被拒绝时必须能让客户端区分"稍后再试"
与"重新分配 tag"，因此自加 code=2。

### 1.5 Bye（任一端点 → 中继，`kind=4`，22 字节）

| 偏移 | 长度 | 字段 |
|---|---|---|
| 6 | 16 | `tag` |

中继收到 Bye 后解除该源地址的端点绑定；若对端仍在线，向对端补发一条 Bye 通知其重新 bind。
GC（空闲 60s / 超 48h）与优雅关闭时也由中继主动向端点发 Bye。Bye 不鉴权（它只能删除"自己地址
对应的端点"，伪造 Bye 至多造成自伤；避免为中继增加对称密钥回传面）。

### 1.6 TCP 降级通道（`21118/tcp`）

每连接先发一帧控制帧完成 bind，之后双向数据帧：

```
u32 len (大端，含 chan 字节) | u8 chan | payload
chan: 0=数据、1=心跳(PING)、2=控制(Bind/BindOk/BindErr/Bye)
```

- 首帧必须 `chan=2` 且 payload 是 79B Bind，握手超时 2s；失败即静默关闭连接。
- 成功回一帧 `chan=2` + BindOk，然后进入数据循环；`chan=0` 载荷按同 tag 转发给对端（可 TCP→UDP、
  UDP→TCP，中继内部按 tag 桥接两种端点），`chan=1` 刷新 `last_seen`，`chan=2` 只接受本 tag 的 Bye。
- 单帧上限 64 KiB（`MAX_TCP_FRAME`）；每连接有界 `mpsc`（256 帧）写队列，由独立写任务串行
  `write_all`，读循环只做转发，避免写阻塞影响读。
- 端点身份显式（连接即身份）：同 tag/role 的新连接直接覆盖旧端点（旧连接收到 Bye 并断开），
  不再套用 UDP 的 `rebind_guard` 判定。

## 2. NAT 重绑定 vs 第三端点（实现裁决）

UDP 中继只能看到源地址，必须区分两种情况：

- NAT 映射失效后换了公网端口（**重绑定**，应允许覆盖）；
- 攻击者/第三个客户端拿同一 tag+role 试图挤进来（**应拒绝**，code=`tag_full`）。

裁决规则（`rebind_guard_ms`，默认 2000ms）：

| 条件 | 处理 |
|---|---|
| 同 role 槽位为空 | 正常占用 |
| 同 role 槽位已占用且地址相同 | 视作重复 bind，刷新 `last_seen`，回 BindOk |
| 同 role 槽位已占用、地址不同、旧端点 `last_seen` 距今 **<** `rebind_guard_ms` | 判为第三端点，回 `tag_full` |
| 同 role 槽位已占用、地址不同、旧端点已静默 ≥ `rebind_guard_ms` | 判为 NAT 重绑定：覆盖地址、`rebinds_total++` |

理由：NAT 映射失效后旧路径已静默，"沉默时间"是可靠证据；而热槽位换地址更像攻击或客户端重复
bind。代价是 NAT 重绑定后最多需等 2s 才能接管（远小于 60s 空闲回收），客户端重 bind 失败时按
指数退避重试即可。

## 3. 与 `design/013-relay-and-p2p.md` 的差异

| 设计文档 | 本实现 | 原因 |
|---|---|---|
| §2.1 "最小信封 + 成对转发" | 控制面 6B 头 + 固定布局；数据包**完全无头**、原样转发 | 数据包加头会破坏客户端 QUIC 报文语义并浪费带宽；靠 magic 前 6B 区分控制/数据足够安全 |
| 只规定 Bind 与 `auth` 公式 | 自定 BindOk/BindErr/Bye 三种应答报文与字段 | 客户端需要知道会话寿命、失败原因，以及被踢/GC 的通知 |
| `tag_full` 一种拒绝 | 自加 `busy`（`max_sessions` 满） | 客户端重试策略不同（换 tag vs 稍后重试） |
| TCP 可选 TLS + ALPN `dfyp-relay-tcp/1` | 仅 TCP 明文最小版，ALPN 未实现（TODO） | 时间预算；见 §4 |
| 中继仅跟踪 `last_seen` | 额外维护 `by_addr` 反查表、nonce 缓存、全局/每 tag 令牌桶 | 转发与阈值判定需要 O(1) 反查与防重放/限速 |
| 设计未规定重绑定语义 | `rebind_guard_ms` 裁决（见 §2） | 无此规则无法区分 NAT 重绑定与第三端点 |
| 设计提到 443 端口 TCP 路径 | 端口可自由配置（默认 21118），无 443 特殊逻辑 | 部署时把 `tcp_bind` 设为 `0.0.0.0:443` 即可，无需代码区分 |
| `max_global_mbps` 可选 | 已实现为全局令牌桶（`Option`） | 设计 §6 特性表要求 |
| 管理面仅 `/healthz`、`/metrics` | 另加 `/status`（JSON），供 `dfy-relay status` 使用 | 运维体验；不改协议 |

## 4. 未实现项 / TODO

1. **TODO(TLS)**：TCP 降级通道目前是明文，未用 rustls 终结 TLS，未协商 ALPN `dfyp-relay-tcp/1`。
   设计里客户端可对中继做 TLS 防 DPI；v1 先保证可用的最小 TCP 版，`DESIGN-NOTES` 在此标注。
2. **allow_list / 白名单**：设计提到可选按 tag 前缀或账号白名单限制接入，v1 只靠共享 `relay_key`。
3. **安装/升级脚本**：`install.sh`、`upgrade`、systemd socket activation 未做（README 给了手工步骤）。
4. **多节点/集群**：v1 单机内存态会话表；水平扩展需要 tag→节点路由与状态同步，未做。
5. **热重载**：SIGHUP 重载配置未做；改配置需重启（短时断连，客户端重 bind 即可）。
6. **指标直方图**：只有计数器/gauge，没有 RTT/包长直方图（客户端侧指标更合适）。
7. **IPv6-only 部署**：`bind` 可为 `[::]:21117`，但未专门测试双栈行为；`admin_addr` 允许 `::1`。
8. **Windows 优雅关闭**：Windows 只有 `ctrl_c`（Ctrl+C/Ctrl+Break），没有 SIGTERM；服务方式部署
   建议用 Windows 服务包装器（NSSM/sc.exe），其停止会走进程终止而非 Bye 广播。

## 5. 并发模型与数据结构

- **单 UDP socket + 单 recv 循环**（不每包 spawn task）：`run_udp` 循环内 `recv_from` → 查表 →
  `send_to`（转发失败只计数）。
- **会话表**：`Arc<Mutex<State>>`，一个进程级 `std::sync::Mutex`。临界区只做哈希查找 + 构造待发
  egress（`Vec<u8>` 预编码），**不 await、不做 IO**，因此在 recv 循环里加锁的代价极小；发送在
  锁外进行。规模目标 ≤ 数千 tag，单锁足够；若未来上万并发会话可改 sharded（按 tag 前缀分片）。
- `State` 内容：`sessions: HashMap<Tag, TagSession>`、`by_addr: HashMap<SocketAddr,(Tag,role)>`、
  `nonces: NonceCache`、`global: Option<TokenBucket>`、`next_id: u64`（端点唯一 id，防止旧 TCP
  连接退出时误删新端点）。
- **TCP**：每连接 1 读任务（spawn）+ 1 写任务（spawn），写任务从 `mpsc(256)` 取预编码帧；
  连接结束 `unbind(tag, role, Some(id))` 并 abort 写任务。
- **GC**：独立 task，1s tick（`MissedTickBehavior::Skip`），清理 idle/超 TTL 会话并向端点发 Bye。
- **限速**：令牌桶 `rate = mbps * 125000 B/s`，`burst = max(rate * 0.1, 64 KiB)`；超限丢包并计数。
- **优雅关闭**：`watch` 通道广播 → 各任务退出 → `drain_all` 给剩余端点发 Bye → `sessions=0`。

## 6. 测试覆盖

- 单元测试 29 个（`cargo test -p dfy-relay`）：HMAC 正/负、时间窗边界、nonce 重放与容量/TTL
  淘汰、Bind/BindOk/BindErr/Bye/TCP 帧布局与解析、tag 表配对/转发/第三端点/NAT 重绑定/GC/限速/
  max_sessions/静默拒绝/Bye 通知、配置解析与 key 格式、令牌桶行为、指标渲染。
- `dfy-relay selftest`（进程内真实 socket，8 步）：双端 bind → 双向 1KB 转发 → 第三端点 `tag_full`
  → 错 key/过期 ts/重放全静默 → NAT 重绑定覆盖旧地址 → per-tag 限速丢弃 → 空闲 GC + Bye →
  TCP↔UDP 桥接。成功输出 `RELAY SELFTEST OK` 且 exit 0。
- `scripts/e2e.py`（Python 3.13 标准库，独立进程验证 RFC 观感的一致性）：绑定两 role、双向 10×1KB
  校验、吞吐粗测、Bye 回收。
- CLI：`config init` 生成可用配置；`config check` 对坏 key / 非回环 admin 报错退出 1；
  `doctor` 对占用端口给 WARN、能探测本机已在运行的 relay；`status` 打印 `/status` JSON。