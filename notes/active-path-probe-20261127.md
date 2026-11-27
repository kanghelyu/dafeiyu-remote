# 候选路径主动探测（2026-11-27）

> 环境：Windows 11 x64，本机 `dfy-relay` + 拥塞代理 `.tmp/dfy-adaptive-controlled-20261126/quic_video_stall_proxy.py`，无强制切换开关。不是两台物理设备/公网。

## 1. 实现

`dfy-path` 的 `Action::RequestProbe(PathKind)` 此前只打印日志。现在 View 收到该动作后会：

1. 在目标路径上建立一条**不承载应用会话**的轻量 QUIC 连接（`probe_path`）；
2. 发 8 个 64B DATAGRAM，读取 QUIC `path.rtt` 与 `lost_packets`/`sent_packets` 增量；
3. 组成 `PathSample` 回填 `PathMonitor::on_sample_for(target, …)`；
4. 返回的决策存入 `deferred_action`，在下一个 250ms 采样 tick 用与常规决策相同的切换逻辑执行。

这样探测不会抢占当前会话的代际、视频发送、剪贴板或文件状态。`recv_bps` 用 `u64::MAX` 哨兵表示“探测没有吞吐量数据”，避免被误判为带宽降档。丢包计算抽成纯函数 `loss_ppm_from_deltas`，有单测覆盖零分母、ppm 上限与溢出饱和。

## 2. 真实验收

- Windows Debug `dfy.exe` SHA-256：`a38df9c715956dc3c5ba8af7d7ec92d56e97faaafdc3bfd176fe99cd82f19ceba`（探测包 1200B 版本）。
- 报告 `.tmp/weak-path-probe-20261127/report.json` SHA-256 `4ae29ea3320df4b48f55b410982eaf9a049a35a942c1bfb72767d1f648cf8009`；View 日志 `.tmp/weak-path-probe-20261127/view.log` SHA-256 `9613d35805c0a5bbbe380595a23340297aac320719aeada5296aac6b4d174c08`。
- 50s 会话：拥塞代理下真实弱网自动迁移到中继（`epoch=1`、恢复确认、帧 #364/364），26,181 DATAGRAM / 27.69 MiB，304 重组 / 304 解码 / 0 解析失败。
- 迁移后 `PathMonitor` 5 次输出 `RequestProbe(Direct)`，1200B 探测样本实测 RTT 约 1.1–1.3ms、loss 0ppm；累计 5 个好样本后按设计输出 `SwitchTo(Direct)`。
- 回切尝试因拥塞代理仍阻断直连握手而失败（`会话迁移握手失败`），保持中继路径；视频统计未受影响。
- 单测：`dfy-path` 新增 `probe_requested_after_cooldown_with_epoch_timestamps`，确认 Unix 毫秒基准下 30s 冷期后仍请求探测；`dfy-cli` 新增 `loss_ppm_from_quic_deltas`。

## 3. 边界

- 本次探测发生在拥塞窗口（首包后 6–22s）结束之后，实测 RTT 3.1–4.2ms 反映当时直连确实已恢复，因此请求回切是正确决策；回切握手失败是测试代理把 Agent 应答仍发给旧直连源地址造成的假象，不是产品缺陷。
- 为覆盖“只对大包敏感”的瓶颈，探测包从 64B 改为 1200B（接近 MTU），重测 50s 会话通过，5 次探测均完成；但没有在拥塞窗口内做 64B/1200B 对照，不把“大包敏感”记为已验收能力。
- 回切失败时保持当前路径，不丢帧进度；没有把“回切失败”误记为迁移成功。
- 探测带来的额外 QUIC 连接会在 Agent 侧等待应用握手至超时，日志中可能出现握手超时记录；不影响既有会话。
- 未测：探测包大小/数量参数标定、中继候选路径的探测、两台物理设备/公网。
