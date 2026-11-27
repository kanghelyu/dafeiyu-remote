# 跨 OS 路径迁移后的业务连续性（2026-11-27）

> 环境：同一台 Windows 11 x64 主机 + Debian 13 QEMU 隔离 guest（TCG，4 vCPU，4 GiB）。**不是两台物理设备，不是公网 NAT/Relay 验收**。
>
> 拓扑：
> - Linux guest：Xvfb `:99`，Agent 监听 `0.0.0.0:24800`，`--auto-path` 备用中继指向 `10.0.2.2:21117`（QEMU user-mode NAT 映射到 Windows 主机上的 `dfy-relay`）。
> - Windows：View 经 hostfwd `127.0.0.1:24800` 直连 Agent，备用中继指向 `127.0.0.1:21117`，用 `--force-switch-once relay` 触发强制迁移。
> - relay tag：`ccddeeff00112233445566778899aabb`；临时口令仅经 stdin，不写入仓库或证据。

## 1. 版本与证据

- Windows Debug `dfy.exe` SHA-256：`ec11b2b5cc1cb66cc8bc97e92f31b174549d8bb3df8484c50a314000d09aa4b6`。
- 报告 `.tmp/clipboard-migration-crosos-20261127/report.json` SHA-256：`e221092daeffd8f46920c756824e6428ef1958b0b52ca148e4be6dbfb4aeda27`。
- View 日志 `.tmp/clipboard-migration-crosos-20261127/view.log` SHA-256：`7d1905e73fb9a7b1f3de7dae8607ae39e4f5469cb98f3d3c6fe88138be8e9bfa`。
- Agent 日志 `.tmp/clipboard-migration-crosos-20261127/agent.log` SHA-256：`e2994b874e583dd91dccd9538333ffec9533e92a18db2bcc1d8ba0a5787696a1`。
- 运行输出 `.tmp/clipboard-migration-crosos-20261127.log` SHA-256：`fb6901a41f20a1e88bdfa5fae5d6556a05957ec9218468cd5bbff4c21d90b773`。

## 2. 结果（通过）

| 项目 | 实测 |
| --- | --- |
| 强制直连→中继迁移 | 成功；`epoch=1`、`恢复确认=true`、`最后帧=#23/23` |
| 迁移后 Windows→Linux 剪贴板 | 35 B Unicode 文本，`Windows→Linux after migration = true` |
| 迁移后 Linux→Windows 剪贴板 | 35 B Unicode 文本，`Linux→Windows after migration = true` |
| View 剪贴板统计 | 清单 1↑/1↓、拉取 1↑/1↓、分块 1↑/1↓、确认 1↑/1↓、字节 35 B↑/35 B↓ |
| View 退出码 | 0 |

结论：同一会话在控制通道整体替换后，双向剪贴板同步仍能继续工作；此前“迁移后剪贴板未稳定复现”的记录不再成立。

## 3. 根因与修复

此前两次失败各有一个独立原因：

1. **测试拓扑错误**：旧脚本把 Agent 和 View 都跑在 Windows 同一台机器、共用同一剪贴板，却去检查 Linux `xclip`；该检查永远不可能成功。本轮改为 Linux Agent + Windows View 的真正跨 OS 拓扑。
2. **迁移握手超时过短**：首次跨 OS 强制迁移出现 `会话迁移握手超时`，Agent 侧随后 `发送 Challenge 失败`。`resume_on_path` 原用 5s 超时，在 QEMU TCG + NAT 路径下新路径握手往返明显超过 5s，把本来可用的迁移误判为失败。已将超时改为 15s（`crates/dfy-cli/src/real.rs`），重测通过。

## 4. 边界

- 强制诊断触发，不是真实弱网自动触发；自动触发的链路另有 `notes/auto-path-acceptance-20261126.md`。
- 同机 QEMU NAT，不是两台物理设备/公网；NAT 下的真实丢包/抖动未复现。
- 15s 是取舍：弱网下更不容易误判，但真实断线时故障检测会变慢；尚未做超时参数的实测标定。
- 在途输入（迁移瞬间正在按键/拖动）仍未专项验收；文件传输的跨迁移哈希一致见 `notes/auto-path-acceptance-20261126.md`。
