# Windows ↔ Linux 跨 OS 验收（2026-11-26）

> 环境：同一台 Windows 11 x64 主机 + Debian 13 QEMU 隔离 guest（TCG，4 vCPU，4 GiB）。不是两台物理设备，不是公网/NAT/Relay 验收。
>
> Linux：Xvfb `:99`，1280×720，XTest/XFixes/RANDR/XInput2；Windows：1470×923，96 DPI。路径为产品 QUIC 直连 UDP，经 QEMU user-mode hostfwd `127.0.0.1:24800`。

## 1. 构建与证据版本

- Linux 端基线源码归档 `dfy-source-crossplatform-20261126.tar` SHA-256：`08a98dd96c8816bfc0e0e1d30f65af197f47513711d8d72abc945676bd6d0037`。
- Windows 端最终源码归档 `dfy-source-linux-controller-20261126.tar` SHA-256：`6a889896b446537d54fa3975cb285223d6d86ed2a6e1517ba356816c50e39915`。两端协议兼容，但 Windows 端额外包含后续输入采集修补，不是同一源码快照。
- Windows Release `dfy.exe` SHA-256：`bca27f261705b627dc9c1041fad17c4c168a0c317f44a77cf59ac3e22c1ae128`。
- Linux Release `dfy` SHA-256：`9b3d94b04c01501bb1303e7833bee72713eebfd7d06b68c41f26d1ef825e4df2`。
- 报告：`.tmp/dfy-crossplatform-live-20261126/report.json`，SHA-256 `60608962f64f93e43daedc7e2bc784e9d70c01329a2cbbf54040c3c477866df8`。
- View/Agent 日志：`.tmp/dfy-crossplatform-live-20261126/{windows-view,linux-agent}.log`。
- 首帧：`.tmp/dfy-crossplatform-live-20261126/linux-first.bgra`，SHA-256 `ae51cb8a59cd4d341dec346064efe679c8856c0ce8370a7379e78bd0a3e38cf6`。

## 2. Windows 主控 → Linux 被控（部分通过）

命令由 `.tmp/crossplatform_accept.py` 执行，临时口令仅经 stdin 传入，不写入仓库/日志：

```text
Linux: dfy agent --host 0.0.0.0 --port 24800 --password-stdin
       --codec mjpeg --fps 60 --bitrate 8000
       --allow-input --allow-clipboard --recv-dir <隔离接收目录>
Windows: dfy view --addr 127.0.0.1:24800 --password-stdin
         --codec mjpeg --seconds 26 --save-first <BGRA>
         --control --clipboard --send-file <51-byte sample>
```

实际结果：

- 进程：View 退出 0；Linux `timeout` Agent 退出 124（计划到时终止）。
- 视频：6,840 DATAGRAM / 7.12 MiB，380 重组 / 380 解码 / 380 关键帧 / 0 解析失败；RTT 1.419 ms；50 个质量反馈窗口。
- 像素：1280×720 BGRA，3,686,400 字节；4,224 个采样点中 3,850 个为红色（91.1458%），首样本 `(254,0,0,255)`，证明画面来自指定 Linux X11 根窗口。
- 指针：Windows 绝对移动经产品采集→QUIC→Linux XTest 注入后，独立 `xdotool getmouselocation` 读回 `(419,309)`，目标 `(420,310)`；View 报告 6 个移动 DATAGRAM，Agent 报告 6 条成功注入 / 0 失败。
- 剪贴板：Windows→Linux 与 Linux→Windows 各同步一个 36 字节 Unicode 文本对象；View 统计两个方向各 1 清单 / 1 拉取 / 1 分块 / 1 确认。
- 文件：51 字节 `crossplatform-sample.txt`，offer/块/确认/结果/完成均 1，失败 0；接收端 SHA-256 `637c617693a739ea24c6d1cfab22fae88f2a27690de01e6753b62b009a392ddd`，与发送端一致。

## 3. Windows 合成键盘/按钮边界（未验证）

本 Windows 环境中，`SendInput`/`keybd_event` 会移动真实光标，但独立最小探针确认合成键盘/鼠标按钮事件不进入：

- `WH_MOUSE_LL` / `WH_KEYBOARD_LL`；
- Raw Input；
- 另一进程的 `GetAsyncKeyState` 持续按下高位。

因此本轮**不把 Windows 主控端合成键盘/鼠标按钮采集记为通过**。轮询光标与正常按键状态的代码已接线，Physical/真实键鼠仍需要在可交互 Windows 桌面复核。Linux→Windows 方向将使用 Linux XInput2 真实事件链验证网络键鼠与 Windows 注入，不能反向替代 Windows 采集端验收。

## 4. Linux 主控 → Windows 被控（通过）

- 环境：同一 Windows 11 x64 主机 + Debian 13 QEMU guest；产品 QUIC 直连 UDP `guest → 10.0.2.2:24803`。使用 24803 是因为 QEMU 已把 24801 转发回 guest，会截获 guest 发往主机的该端口。
- 源码归档 `dfy-source-linux-controller-20261126.tar` SHA-256：`c00ec7ce1e0c47dd8f4e97569c1d5f5ec1c5d06e1c61c7eca44364886ee280b`。
- Linux Release `dfy` SHA-256：`56750d5fbe0f6e932b6792e469ddf4aed24fa67fb49550db9493e740c4526a8d`；Windows Release `dfy.exe` SHA-256：`73096baa178170d9b41a9873c4b0277816d21e31c46673dbcd96307988265399`。
- 报告：`.tmp/dfy-linux-controller-live-20261126/report.json`，SHA-256 `d56a17ac4d2a2c60462f0921fd1fb2980ed7932b10682788631bad67cc1fdc9f`；运行日志 `.tmp/linux-controller-accept-final-20261126.log`，SHA-256 `a4b93b765a582eb9e10009baa05cf19fc556c56073072e5e1715d0dc0b8507a0`。
- 视频：26 秒 36,852 DATAGRAM / 39.08 MiB，204 重组 / 204 解码 / 329 关键帧 / 0 解析失败，RTT 2.019 ms；首帧 2940×1846，4,101/4,356 采样非黑，SHA-256 `031c6b6f8709e9fdb0117ee1b034b6b75dbad2311c450be98dfab51d7fc4182a`。画质从 2940×1846/60fps 自适应降到 760×476/15fps。
- 输入：Linux XInput2 优先、X11 根窗口核心事件降级的控制端采集到真实 `xdotool` 事件；独立 `GetCursorPos` 读回 `(736,462)`（目标 `(735,462)`），View 报告 2 个移动 DATAGRAM / 8 个其它事件，Windows Agent 报告 10 条注入 / 0 忽略 / 0 失败；探针记录到 `KEY:A`。探针 UI 未记录 `MOUSE:Left`，因此按钮到达目标应用的最终效果仍需物理桌面复核。
- 剪贴板/文件：双向 Unicode 文本各 45 B；50 B 文件 offer/块/确认/结果/完成各 1，失败 0，接收端 SHA-256 `c6105bbb09bac4e27ea50bb75edaf835114349dc41f1e1decdbf93f5b6e920df`，与发送端一致。
- 边界：不是两台独立物理设备，不是公网 NAT/Relay；真实桌面焦点、组合键、IME、弱网、长稳、macOS/Android 真机仍未验收。

## 5. 不能外推为

- 两台独立物理设备、公网 NAT 打洞或 Relay 路径通过；
- 真实弱网、长稳、120fps/12Mbps、硬件编解码或首帧延迟通过；
- Linux Wayland、macOS、Android 真机/跨机通过；
- 完整产品 UI、权限通知、会话迁移或生产发布验收通过。
