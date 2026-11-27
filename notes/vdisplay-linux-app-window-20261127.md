# Linux 虚拟显示：真实 DPI 与真实应用窗口（2026-11-27）

## 范围

- `XvfbBackend::xvfb_command` 现在把 `VirtualDisplaySpec::dpi` 作为 `-dpi <N>` 传给 Xvfb（DPI 只能在启动时设定，运行时无法改；refresh_hz 仍是元数据，未伪装成真实刷新率）。
- 单测更新：命令参数含 `-dpi 120`；所有权校验仍按 `-screen 0 WxHx24` + `:N` 识别，不被新增参数破坏。

## 真实验收

环境：同一 Windows 11 x64 主机 + Debian 13 QEMU guest（TCG），Linux Agent 创建隔离 Xvfb，Windows View 经 QEMU hostfwd 收流。

1. **DPI 真生效**：Agent `--virtual-displays 1 --virtual-width 640 --virtual-height 480 --virtual-dpi 120`，`xdpyinfo -display :106` 回报 `dimensions: 640x480 pixels`、`resolution: 120x120 dots per inch`；`dfy vdisplay list` 显示 `120DPI`。报告 `.tmp/vdisplay-dpi-20261127/report.json` SHA-256 `baaad641ff4d305d6c2f4cccc9646cbe7a5c52f33bd25afab0933b51be65370`。
2. **真实应用窗口**：Agent 创建 `--virtual-dpi 144` 的 `:108`，先在无应用时抓首帧，再在该 display 上启动 `xmessage -geometry 320x120+120+120 'DFY VDISPLAY APP TEST'`，再抓首帧。两次 View 均退出 0、首帧均 640×480 BGRA（1,228,800 B）；对比 **40,181 像素发生变化**，应用帧非黑像素同为 40,181（基线为纯黑根窗口）。基线 SHA-256 `10f4d37bc929077c1d41b064466013afdccb783a7fb766897ecf6f85d84b63f0`，应用帧 SHA-256 `7f2e5ff7aef81eb9818aa37c6a2d157d4c8841f065864e802efe853bd660ea3f`，报告 `.tmp/vdisplay-app-20261127/report.json` SHA-256 `6f95db4cc140dc7202ef9b7743a9c708c9f884d364737a2c69ee2dae4bd6c346`。

## 虚拟屏内应用输入（未验收）

尝试用 Windows 主控 `--control` 把鼠标移动经 QUIC 注入到虚拟屏上的 xmessage：View 报告“控制输入已启用”，但 12s 会话内 `鼠标移动 DATAGRAM = 0`。诊断发现本机 Python 进程与 View 进程读到的 `GetCursorPos` 坐标不一致（前者 (29,16)，后者 (599,404)），外部合成事件无法作为 View 的可观察坐标变化输入，因此**不把虚拟屏内应用输入记为通过**。这与既有“Windows 合成键钮不可观察”的边界一致，需要可交互桌面复核。

## 边界

- 只验证 Linux/Xvfb；Windows IDD 与 macOS 原生后端未因此改变。
- `refresh_hz` 仍是传入元数据，不是 Xvfb 真实刷新率；没有动态刷新率验收。
- 应用窗口用 xmessage（X11 原生应用），不代表 Wayland/复杂 GUI 应用；没有输入到该窗口的交互验收。
- 同机 QEMU，不是两台物理设备/公网。
