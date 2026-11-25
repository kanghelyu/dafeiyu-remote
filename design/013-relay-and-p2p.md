# 中继与 P2P 架构设计（v1）

> 业主三决策之一：中继不是"简单流量转发程序"；P2P 优先、质量检测、失败/劣化时**可恢复地迁移**到中继；中继服务器由用户自配、可无限添加、统一池化选择。中继部署必须极简。
>
> 本文只写机制（clean-room 自研），不复制任何第三方实现。

## 1. 三条连接路径

| 路径 | 说明 | 何时使用 |
|---|---|---|
| Direct-P2P | 双方经 STUN 拿到 srflx 候选，UDP 打洞后 quinn 直连 | 默认；质量达标则一直用 |
| Relay-UDP | 双方把 quinn 对端地址设为同一台中继的固定端点，由中继按 `tag` 成对转发原始 UDP 密文 | P2P 打不通 / 质量差 / 对称 NAT / 防火墙 |
| Relay-TCP443 | 中继提供 TLS/TCP 入口，QUIC 不适用时降级为帧复用（语义等价） | UDP 全被封的企业网络 |

**关键洞察（决定实现成本）**：中继**不解析 QUIC、不终结 TLS**。它只做 `tag → (endpointA, endpointB)` 的**成对 UDP 转发**，两端把中继地址当作固定"会合点"。于是：
- 端到端加密天然成立（中继只见 QUIC 密文）；
- 双方看到的对端地址始终是中继地址 → QUIC **无需迁移**即可稳定运行；
- 中继实现量 ≈ 1k 行 Rust（会话表 + 转发 + 鉴权 + 限速 + 统计 + 健康检查）。

## 2. 中继数据面协议（DFYP/1-Relay）

### 2.1 会合（bind）
```
Client ──UDP──► Relay: BindRequest{tag(16B), role(0=发起/1=应答), auth(HMAC), nonce}
Relay ──UDP──► Client: BindOk{tag, peer_pub_addr(仅调试用，默认隐藏), expires}
```
- `tag` 由会话双方从 `session_id + 随机 salt` 派生（HMAC/RDV 下发），48h 过期；双方都 bind 后中继建表即开始转发。
- 未 bind 的包、tag 不符、鉴权失败 → 静默丢弃（不回错，防探测）。
- 每个 tag 最多 2 个端点；第三端点 bind 被拒绝（`tag_full`）。
- 转发：收到 A 的包 → 转发给 B，反之；**不改包体**（只换 IP/UDP 头）。
- 保活：无流量 20s 后发 1B 心跳（共享 ping 端点），60s 无流量回收 tag。

### 2.2 鉴权
- 中继配置 `relay_key`（32B）；客户端档案保存同一 key。
- `BindRequest.auth = HMAC-SHA256(relay_key, tag || role || nonce || timestamp)`，±300s 时窗，nonce 去重（防重放）。
- 可选中继侧 `allow_list`（设备 ID 前缀 / 账号），默认关闭（个人自建不折腾）。
- 中继**不知道**设备私钥、会话密钥；冒充中继无法解密，但可拒绝服务 → 客户端可配置"多中继并测"。

### 2.3 加密与隐私
- 会话内容：QUIC/TLS1.3 端到端，中继零知识。
- 元数据：中继可见 `tag`、两端 IP、包数/字节/时间——与 TURN 相同；提供 `--no-log-ips`（不落盘 IP）。
- 中继绝不做"内容缓存/录制/转码"。

### 2.4 TCP443 降级通道
- 两端各自与中继建 TLS/TCP（ALPN `dfyp-relay-tcp/1`），帧格式 `u32 len | u8 chan | payload`，`chan` 0=数据（内含原 QUIC 包）、1=心跳、2=控制。
- 中继按 tag 成对转发 `chan 0` 帧；语义与 UDP 路径一致，仅性能差、可穿透 UDP 封锁。

## 3. P2P 打洞与候选

1. **RDV 信令**（WSS）交换候选：`host`（可选，仅同网段）、`srflx`（STUN 获取）、`relay`（中继 tag 绑定后生成）。
2. 打洞：双方对 srflx 候选并发发 QUIC Initial（不同的本地 socket），任一成功即 P2P 建立；失败 1.5s → relay 路径。
3. 对称 NAT 检测：STUN 返回的映射端口变化 / 打洞失败 → 直接走 relay，不再重试。
4. 局域网：mDNS/本地广播发现可选（P1），MVP 用手动 IP 或 RDV。

## 4. 质量检测与路径切换（核心）

### 4.1 指标采集（`stats` 通道 2Hz）
RTT（QUIC RTT）、丢包率、可用带宽（GCC/REMB 估计）、发送码率、帧率、卡顿次数、jitter buffer 水位。

### 4.2 策略（可配置）
- **P2P 保持条件**：RTT < `p2p_rtt_max`（默认 120ms）、loss < 3%、无连续 2s 卡顿。
- **劣化判定**：连续 3 个采样（1.5s）不满足 → 进入 `degraded`，开始预迁移（并行预热中继路径，不中断当前画面）。
- **失败切换**：P2P 连接 1s 内无数据或有 QUIC 错误 → 立刻迁移。
- **回切**：Relay 稳定 30s 且探测 P2P 可用（后台低频试探）→ 可选回切（默认关闭，避免抖动）。

### 4.3 会话迁移协议（不断流）
1. 双方已有活跃会话 `S`（TLS 会话票据、`session_token`、`last_seq`）。
2. 劣化时：控制通道发 `PrepareMigrate{target: RelayUDP{tag} | Direct{candidates}, nonce}`；双方并行走新路径握手（**0-RTT + TLS session resumption**）。
3. 新连接上第一条控制消息 `Resume{session_token, last_media_seq, last_input_seq, state_digest}`。
4. 服务端校验 token（HMAC，一次性 + 30s 有效）→ 回复 `ResumeOk{resume_seq}`；旧连接保持 2s 双发（媒体只接受新路），随后关闭。
5. 视频侧：新路径立即 `RequestKeyframe`；剪贴板/文件任务按 `seq` 续传（已确认块不重传）；输入侧发送"当前修饰键状态快照"防卡键。
6. 失败回滚：新路径 1.5s 未完成 → 保持旧路径并退避 5s。

> 效果：画面冻结 ≈ RTT + 1 帧（局域网 <80ms，跨网 <300ms），用户感知为"轻微卡顿"，非"断开重连"。

## 5. 中继服务器池（客户端侧）

```toml
# relay profiles（客户端配置，可导入导出）
[[relay]]
name = "上海-自建"
addr = "relay.example.com:21117"
key  = "base64:..."        # relay_key
tls  = "auto"              # auto | required | off（TCP443 通道用）
region = "cn-east"
priority = 10              # 越小越优先
enabled = true
```

- 策略：`manual`（只用指定）、`lowest-latency`（并发 ping 测 RTT 取最优）、`preferred-first`（首选可用就用，失败降级）、`failover`（默认：按 priority 试，失败换下一个）。
- 健康：客户端每 30–60s 对池内中继做 bind 探测（成功/失败/RTT/带宽），UI 显示状态灯。
- 切换中凭据保护：bind 鉴权只含 HMAC，不发送 relay_key 明文；`tls=required` 时先校验证书。
- UI 必须常显"当前路径：P2P / 中继(名称)"与 RTT/丢包。

## 6. 中继服务端（Linux）

### 6.1 单二进制 `dfy-relay`
- 子命令：`serve`（默认）、`config init`（交互向导）、`config check`、`status`、`doctor`（端口/防火墙/TLS 诊断）、`version`。
- 默认端口：UDP 21117（数据）、TCP 21118（TLS 降级）、127.0.0.1:21119（本机管理/指标，不对外）。
- 特性：并发会话上限、按 tag 限速（每会话 Mbps）、全局带宽上限、连接数/流量 Prometheus 指标（`/metrics`，仅本机/可配 token）、优雅关闭、SIGTERM、日志 JSON（可选 syslog）、健康检查（`/healthz`）。
- 资源：空载 <30MB 内存；1Gbps 转发在 4 核现代 x86 上 CPU <40%（目标，需实测）。

### 6.2 极简部署（业主强调）
目标体验：5 步、1 分钟、无需理解 NAT/TURN。
1. **一键脚本** `install.sh`（Ubuntu/Debian/CentOS 通用）：检测架构/端口/防火墙 → 下载单二进制 → 生成 `relay_key` → 写 `/etc/dfy/relay.toml` → 装 systemd unit → 启动 → 打印"客户端填这个地址 + 这个密钥"（含二维码）。
2. 交互向导 `dfy-relay config init --interactive`（同脚本非 root 场景用）。
3. 自动 TLS：默认自签（指纹随二维码给出）或用户提供证书；`--letsencrypt domain` 可选。
4. 防火墙诊断：`dfy-relay doctor` 检查 UDP/TCP 端口连通、SELinux、云厂商安全组提示。
5. 升级：`dfy-relay upgrade`（下载校验签名→原子替换→重启）；卸载：`uninstall.sh`。
6. Docker：官方镜像 + `docker-compose.yml`（含 healthcheck、只暴露 21117/udp 21118/tcp）。
7. 文档：端口、协议、防火墙、资源需求、常见故障（nat 不支持、端口被封、TLS 拦截）。

## 7. 与桌面应用的耦合
- 桌面端通过 `dfy-nat`/`dfy-session` 使用中继，不感知细节；`dfy-relay` 与桌面端共享 `dfy-proto` 的信封定义但独立发布（体积/依赖最小化）。
- 自建 RDV 可选：MVP 允许"直连 IP + 中继 tag 手动输入"完全离线；RDV 仅用于设备列表/在线态/候选交换（Phase 2+）。

## 8. 验收（中继）
1. `dfy-relay selftest`：本机起 relay + 两个 loopback 客户端，验证 bind/转发/保活/回收/鉴权失败。
2. 跨机测试：两台 NAT 后机器经公网中继互通，1080p60 画面可用；拔掉 P2P（封锁 UDP）后 1.5s 内自动迁移，画面 <300ms 恢复。
3. 压测：单会话 50Mbps 转发 10 分钟无丢包累积；100 并发会话内存/CPU 曲线记录。
4. 部署验收：干净 Ubuntu 22.04 VPS 上 `sh install.sh` 后 60s 内可被客户端连接；`doctor` 全绿；重启/升级/卸载各一次。
5. 安全：错误 key/过期 tag/重放 bind 全部被拒；抓包确认中继无明文。

## 9. 待定
- 中继集群化（多地域、tag 路由、跨集群转发）→ Phase 4。
- 是否支持 TURN 兼容（复用企业既有 TURN）→ 可作 Phase 5 生态项。
- P2P 后台探测频率与耗电平衡（移动端）。