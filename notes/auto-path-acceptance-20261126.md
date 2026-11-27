# 可恢复路径迁移验收（2026-11-27）

## 范围

本轮把已实现的 `dfy-path` 决策器、`dfy-transport::client_resume_handshake` 与 `dfy-session` 的共享采集/会话代际机制接入 `dfy-cli`，实现了 **质量驱动的 P2P/直连 ↔ Relay 可恢复会话迁移** 的第一条完整路径。

- `agent --auto-path`：同时绑定直连 UDP 端点与中继 QUIC 端点，共用同一 TLS 身份、`ServerAuth`/`ResumeRegistry`、`SessionCoordinator` 和 `CaptureHandle`；两个端点的连接都走 `server_handshake_context` + `serve_agent_shared`，因此中继连接可作为 pending lease 接管发帧。
- `view --auto-path`：初始走直连，250ms 采样 `PathSample`（RTT、接收速率、stall），`PathMonitor` 按默认阈值输出 `Action`；`SwitchTo` 时用旧 `Welcome`/设备签名密钥/最后完整帧进度执行签名 PREPARE→COMMIT 恢复握手，成功后替换本地连接/channel/心跳，保留解码器、重组器状态机、UI、输入、剪贴板与文件队列。
- `--force-switch-once direct|relay` 是诊断参数，用于在首个采样周期强制触发一次迁移，避免依赖真实弱网才能验证协议与接线。
- 旧的 `SessionResumeOk` 由 `run_client` 校验；Agent 侧旧 lease 收到 `changed()` 后以 `migrated` 退出，新 lease 接管共享帧序号并请求关键帧。

## 自动测试

- `cargo test -p dfy-cli --bins`：44 通过 / 0 失败。
- `cargo clippy -p dfy-relay-profiles -p dfy-cli --all-targets -- -D warnings`：通过。
- `dfy-path`、`dfy-session`、`dfy-transport` 既有的确定性状态机、live capture、真实 QUIC resume 测试未改动并继续作为底层证据。

## 真实链路

Windows 11 x64，同一物理主机；`dfy-relay` 监听 `127.0.0.1:21117`，TCP 降级关闭，admin 仅回环；`dfy.exe` Debug SHA-256 `6ae85477925606eb440e43f75c58590f0ced94c4c8bf1dd7392a400462d7129c`。

命令要点：

```text
dfy agent --host 127.0.0.1 --port 24850 --password-stdin --codec mjpeg --fps 30 --bitrate 2000 \
  --auto-path --relay-profile local --relay-tag aabbccddeeff00112233445566778899
dfy view --addr 127.0.0.1:24850 --password-stdin --codec mjpeg --seconds 12 \
  --auto-path --relay-profile local --relay-tag aabbccddeeff00112233445566778899 \
  --force-switch-once relay
```

实际结果：

- View 退出 0；12 秒 4,868 DATAGRAM / 5.17 MiB，13 重组 / 13 解码 / 25 关键帧 / 0 解析失败，RTT 0.758ms，18 个反馈窗口。
- View 日志：`会话已迁移到中继（epoch=1, 耗时 410ms, 帧=#0）`；`路径迁移：次数 = 1，恢复确认 = true，最后帧 = #25/25`。
- Agent 日志：旧会话 `end_reason="migrated"`（帧 1），新会话接管后累计帧 25；`会话迁移已提交，新连接接管发帧` 证明 ownership/fencing 生效。
- 迁移后画质仍走原有自适应闭环：`2940×1846/30fps → 2350×1474/24fps → 1878×1178/19fps`，说明运行时采集/编码状态跨路径保留。
- 报告 `.tmp/auto-path-live-20261126/report.json` SHA-256 `611b7adf75a95d277bcfac0c6a53ec8d003baef25dc98a59cb3d0716697813541`；运行日志 SHA-256 `1ce2e1b0ed330933fd9dd55e95fe0708c5dfc2074eaffab4d5a63228093b25fa`。

### 反向回切：中继 → 直连

新增 `--auto-initial-path relay`，使 View 可以从中继起步，再用 `--force-switch-once direct` 验证回切：

- View 退出 0；12 秒 7,226 DATAGRAM / 7.68 MiB，22 重组 / 22 解码 / 0 解析失败，RTT 0.686ms。
- `已连接 （经中继）` → `会话已迁移到直连（epoch=1, 耗时 453ms）`。
- `路径迁移：次数 = 1，恢复确认 = true，最后帧 = #22/22`。
- Agent 旧中继会话 `reason="migrated"`，新直连会话接管并继续到第 22 帧。
- 报告 `.tmp/auto-path-reverse-live-20261126/report.json` SHA-256 `815c0a933d93cca72c20bc5716cba4ec58cd64282e7083b30c945ac69fc6f6bf`；运行日志 SHA-256 `6cb87fca0e2ae2b8dfd2b5540c9757d2c1842d509f67f692054a7d8e914c4103`。

### 真实弱网自动触发（无强制开关）

复用已验证的 QUIC 视频拥塞代理：View 初始连接代理 `127.0.0.1:24446`，代理转发到 Agent `127.0.0.1:24443`，6–22 秒区间把视频大包延迟约 2.4 秒；不使用 `--force-switch-once`。

- View 退出 0；32 秒 15,725 DATAGRAM / 16.68 MiB，101 重组 / 101 解码 / 132 关键帧 / 0 解析失败，55 反馈窗口。
- 代理启动后约 9 秒，`PathMonitor` 因持续 stall 自动触发 `SwitchTo(Relay)`：`会话已迁移到中继（epoch=1, 耗时 512ms, 帧=#9）`。
- `路径迁移：次数 = 1，恢复确认 = true，最后帧 = #136/136`；帧进度跨迁移连续。
- 迁移前画质因拥塞降到 1878×1178/19fps，迁移后恢复并继续自适应到 1490×934/19fps。
- 报告 `.tmp/weak-path-live-20261126/report.json` SHA-256 `f9ad5fcbdcb5a6882f46e580e89061cad9a6be31e9c5de98fbc980b626b9334f`；运行日志 SHA-256 `37bd2ab8d169ccf97e076022e3b634b397afa418c69561cf4a00dad41b494060`。

### 迁移期间文件传输连续性

强制直连→中继迁移会话中同时开启 `--clipboard --send-file`：View 在 migration 前发起 44B 文件 offer，迁移在约 451ms 后完成，接收端最终得到同哈希文件（SHA-256 `5ed20e9d4f5f35a9e5b0fcf36e8f41aeb131bd486837ab76e0ce5ea4074e2b92`）。证据 `.tmp/inflight-migration-live-20261126/incoming/migration-file.txt`。剪贴板在迁移后的同会话复测未稳定复现，暂不计为迁移期业务状态恢复通过。

## 边界

- 双向迁移与真实拥塞自动触发均已用本机代理/中继验证；仍是隔离回环，没有两台物理设备或公网 NAT/Relay 证据。
- `RequestProbe` 分支目前只记录日志，未做主动探测包的独立质量采样；丢包 ppm 暂填 0，带宽由接收字节差估算。
- 自动迁移期间没有验证输入/剪贴板/文件的在途消息重放（协议层 resume 只保证会话/帧进度）；这是下一轮待补的业务状态恢复测试。
- 同机回环，不是两台物理设备或公网 NAT/Relay 验收。
