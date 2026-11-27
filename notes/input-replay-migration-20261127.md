# 迁移瞬间按住输入状态的跨会话恢复（2026-11-27）

> 环境：同一台 Windows 11 x64 主机 + Debian 13 QEMU 隔离 guest（TCG）。**不是两台物理设备/公网**。
>
> 拓扑：Linux guest 主控（`xdotool keydown a` 产生真实 XInput2 事件）→ Windows 被控（`--allow-input`）。QEMU NAT 上直连 `10.0.2.2:24803`，备用中继 `10.0.2.2:21117`，`--force-switch-once relay`。口令仅经 stdin。

## 1. 实现

- `InputCapture` 的 Windows 钩子线程与 Linux X11 采集线程各自把“当前按住的 HID 键 Usage / 鼠标按键编号”发布到一个共享 `HeldInput` 快照。
- 强制/自动迁移成功后，View 在**新控制通道**上为每个仍按住的键/按钮重放一条 Down 事件，并打印 `迁移后已在新通道重放按住的输入状态 replayed=N`。
- Agent 侧 `InjectHandler` 维护按下集合：重复 Down、无主 Up 不再二次注入；`release()` 清空集合。重放因此是幂等的，不会造成重复按键。

## 2. 真实验收

- Windows Debug `dfy.exe` SHA-256 `e9cf26376f967f7954d08877f13b8ff07805cd7ac934880994052efcc30ac856`；Linux guest Debug `dfy` 由当日同步源码构建。
- 报告 `.tmp/input-replay-migration-20261127/report.json` SHA-256 `7a75a9513862b49089b2b7e8b4a03e1903e6c3fbd5b9abce814d33f7c1ac7ac`；View 日志 SHA-256 `77ca96602dcdcb6aad4c0d50a50dad6a7017ddf16f0628dda37bdf9ba88dd80c`；Agent 日志 SHA-256 `871dba8a88f067d9b7ad408891255c6645c7160d542de8ce59ece3a4481e39ff`。
- 结果：`view_exit=0`、`migrated=true`、`resume_ok=true`、`帧#26/26`；`held_input_replayed=true`，日志 `replayed=1`（迁移前按住的 `A` 键在新通道重放）。
- Agent 侧旧会话 1 条输入注入 / 0 忽略 / 0 失败，新会话 233 条 / 0 忽略 / 0 失败；未观察到注入错误或卡键。
- 单测 `input_bridge::tests::replayed_key_and_button_down_are_idempotent` 验证重复 Down/Up 去重与 `release()` 后重新按下。

## 3. 边界

- 只重放键/按钮，不重放鼠标绝对坐标；迁移瞬间未发出的移动 DATAGRAM 仍未专项验收。
- Windows 合成键事件在本 harness 不进入低级钩子，故只验证了 Linux 主控→Windows 被控方向；Windows 主控→Linux 被控方向的按住重放仍需可交互桌面。
- 强制诊断触发，不是真实弱网自动触发；同机 QEMU NAT，不是两台物理设备/公网。
- 重放会让被控端收到一条重复 Down，依赖幂等去重；若对端不是本产品实现，该重复 Down 语义未约定。
