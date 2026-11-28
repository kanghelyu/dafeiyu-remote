# 大肥鱼远程 / Dafeiyu Remote

> 一个完全自研、跨平台（Windows / macOS / Linux / Android）的远程桌面与设备协作工具。
> 低延迟串流、P2P 直连、键鼠/剪贴板/文件互通，从协议到驱动全部干净实现。

**状态：早期开发中（v0.1 roadmap → MVP = Win↔Win 全链路）。** 暂无可用发行版，欢迎围观与提 issue。

## 为什么做它

现有远控产品要么闭源、要么四端体验割裂；我们希望有一套**协议公开、实现可审计、四端一致**的远控基础设施：

- **快**：QUIC + DATAGRAM 承载视频/音频，应用层 FEC，自适应码率，目标 1080p60 / 局域网 4:4:4。
- **直连**：自研 ICE-lite 打洞 + 自有 relay 中继兜底，不依赖第三方信令。
- **安全**：TLS 1.3 + 设备 Ed25519 身份绑定，relay 零知识；默认最小权限。
- **全平台**：Windows / macOS / Linux 作为被控与主控；Android 作为主控（被控后续版本）。
- **可扩展**：能力矩阵协商，新老客户端可混连；协议规范独立发布，鼓励第三方实现。

## 架构一览

```
┌──────────── Client (主控) ────────────┐        ┌──────────── Agent (被控) ────────────┐
│ UI(Flutter)  ← 外部纹理视频帧          │        │ 采集(DXGI/ScreenCaptureKit/PipeWire) │
│ dfy-session (会话/输入/剪贴板/文件)     │  QUIC  │ 编码(NVENC/AMF/QSV/VideoToolbox)     │
│ dfy-transport (quinn/TLS1.3/DATAGRAM)  │◄──────►│ dfy-input (SendInput/VHF/…)          │
└───────────────┬───────────────────────┘        └───────────────┬──────────────────────┘
                │ 信令(WSS)/候选交换                               │
        ┌───────▼────────┐                                ┌───────▼────────┐
        │ RDV 信令/ID 服务 │◄──── relay（零知识中继，UDP） ──►│ RDV / relay    │
        └────────────────┘                                └────────────────┘
```

- 协议规范：[`spec/DFYP-v1.md`](spec/DFYP-v1.md)（Apache-2.0 / CC-BY-4.0 发布）
- 代码实现：Rust 内核（`crates/`）+ Flutter 四端 UI（规划中）

## 目录

| 路径 | 内容 |
|---|---|
| `crates/` | Rust workspace：协议、传输、NAT、采集、编解码、输入、剪贴板、文件、虚拟屏、会话、CLI 与 `dfy-manager` 图形面板 |
| `spec/` | 自研协议规范（DFYP/1） |
| `design/` | 产品与架构设计文档 |
| `apps/` | 四端 UI（Flutter，脚手架规划中） |
| `research/` | 逆向/调研笔记（**仅本地保留，不随仓库发布**） |

## 图形化管理面板

`dfy-manager` 使用 eframe/egui 原生桌面部件，提供设备、Agent、中继、虚拟显示和诊断五页。
设备页支持地址/口令直连、收藏与搜索；Agent 页支持受控启停；中继和虚拟显示操作在后台执行，
口令只经 stdin 传给子进程，不写入设备书签。默认窗口为 1120×720，窄窗口自动折叠侧栏。

```bash
cargo run -p dfy-manager -- --config ./dfy-relay-profiles.toml --devices ./dfy-devices.toml
# 直达指定页面：devices / agent / relays / displays / diagnostics
cargo run -p dfy-manager -- --config ./dfy-relay-profiles.toml --tab diagnostics
```

Windows 11 已实测五页渲染、设备保存和本机 Agent 启停；Relay 完整 GUI 流程、窄窗口/多 DPI、
Linux/macOS 交互桌面及 Rust 1.75 依赖构建仍未验收，详见 `notes/manager-ui-20261127.md`。

## 文件传输（控制端 → Agent）

控制端把本地文件/目录发给 Agent，Agent 落到指定目录；协议见 `design/024-file-transfer.md`，
v1 复用 control 流（256 KiB/块，按 16 MiB 或 64 块回 Ack，BLAKE3 校验，原子落盘 `.dfy-part` → rename）。

```bash
# Agent：开启接收目录（不存在会自动创建）
printf '%s\n' 'example-passphrase-change-me' | cargo run --release -p dfy-cli -- agent --port 21116 --password-stdin --recv-dir ./incoming

# 控制端：发送文件（可多次 --send-file，目录会被展开递归发送）
printf '%s\n' 'example-passphrase-change-me' | cargo run --release -p dfy-cli -- view --addr 127.0.0.1:21116 --password-stdin --seconds 15 \
  --send-file ./report.pdf --send-file ./screenshots/
```

实测（Windows 本机回环，直连）：3 MiB 随机二进制 + 54 B 文本 → `文件：offer 2，块 13（3.00 MiB），确认 2，结果 2，完成 2，失败 0`，
落盘两个文件 SHA-256 与源文件一致。

## 音频（实验性）

```bash
# 被控端：16kHz/单声道/20ms PCM 经音频 DATAGRAM 发送
dfy agent --audio-out
# 主控端：收到后写入默认输出设备
dfy view --addr 127.0.0.1:21116 --audio-in --volume 50 --mute
```

无输入设备的机器可用 `--audio-out --audio-test-tone` 发合成正弦波验收链路；
这**不是**真实麦克风采集。当前没有抖动缓冲、丢包补偿或回声消除。

## 经中继连接（防火墙友好的兜底路径）

当两端都在 NAT 后面、UDP 打洞不可行时，用一台有公网 IP 的机器做中继。
中继是**零知识**的：它只按 tag 把两个 UDP 端点的包互相转发，不解析 QUIC、不终结 TLS，
因此端到端加密不受影响。

```bash
# 1) 在中继机生成配置并启动（Linux/Windows 均可）
dfy-relay config init --out relay.toml
dfy-relay serve --config relay.toml

# 2) 被控端：指定中继地址、relay_key 与两端共享的随机 tag
printf '%s\n' 'example-passphrase-change-me' | dfy agent --relay relay.example.com:21117 \
  --relay-key 'base64:<relay_key>' --relay-tag '00112233445566778899aabbccddeeff' --password-stdin

# 3) 主控端：同样的中继参数；显式启用剪贴板
printf '%s\n' 'example-passphrase-change-me' | dfy view --relay relay.example.com:21117 \
  --relay-key 'base64:<relay_key>' --relay-tag '00112233445566778899aabbccddeeff' --password-stdin --clipboard
```

`--relay-tag` 在使用 `--relay` 时必填，并不再从会话口令派生。实际部署应使用随机生成的 16 字节 tag；
口令至少 16 字节，推荐用 `--password-stdin` 或交互式隐藏输入，不要使用命令行明文参数。

实测（本机回环中继、6 秒）：视频 DATAGRAM 359、重组帧 60、关键帧 1、解析失败 0、
RTT 0.41 ms；剪贴板 24.55 KiB 分块闭环。设计细节见 `design/022-relay-client.md`。

## P2P 直连（UDP 打洞）

两端都能出 UDP 时优先打洞直连，失败再回退中继。打洞流程：STUN 探测公网映射 →
交换候选 → 双端互发打洞帧 → 把已连通的 socket 交给 QUIC（TLS/会话语义与中继路径完全一致）。

```powershell
# 被控端：固定本端端口 + 对端候选（对端候选也可留空，运行时从 stdin 粘贴）
dfy agent --p2p --peer <对端IP:端口> --p2p-port 21120 --punch-timeout-ms 8000

# 主控端：同样给对端候选
dfy view --p2p --peer <被控IP:21120> --p2p-port 21121 --seconds 10
```

- 有公网 STUN 时用 `--stun stun.example.com:19302` 获取 ServerReflexive 候选（可逗号分隔多个）。
- 注意：候选只枚举本机网卡 IP，**不能用 `127.0.0.1` 当对端**（socket 绑在网卡 IP 上收不到回环包）；
  同机测试请用网卡 IP（例如 `172.18.0.1`）。
- 本机回环实测：`打洞成功：本端 0.0.0.0:21121 ↔ 对端 172.18.0.1:21120` → QUIC 握手成功、RTT 0.548 ms。
  设计细节见 `design/023-p2p-punch.md`。

## 构建与自测

前置：Rust stable（Windows 建议 `x86_64-pc-windows-gnu` + mingw-w64，或 msvc + VS Build Tools）。

```bash
cargo build --workspace
cargo test --workspace
cargo run -p dfy-cli -- selftest      # 端到端回环自测：QUIC 握手 + 视频/输入 DATAGRAM
```

两个进程手动对跑（协议 demo，无真实采集）：

```bash
cargo run -p dfy-cli -- demo-agent --port 21116 --password-stdin
cargo run -p dfy-cli -- demo-client --addr 127.0.0.1:21116 --password-stdin
```

**真实屏幕串流**（Windows：DXGI 采集 + Media Foundation H.264；其它平台：MJPEG 兜底）：

```bash
# 被控端：采集本机屏幕并推流（Ctrl+C 退出）
cargo run -p dfy-cli -- agent --port 21116 --password-stdin --monitor 0 --fps 30 --bitrate 8000

# 主控端：连接、解码、打印统计，可选把首帧存成裸 BGRA
cargo run -p dfy-cli -- view --addr 127.0.0.1:21116 --password-stdin --seconds 10 --save-first first-frame.bgra

# 再加控制输入与双向剪贴板；Agent 也必须显式启用对应能力
cargo run -p dfy-cli -- agent --port 21116 --password-stdin --allow-input --allow-clipboard
cargo run -p dfy-cli -- view --addr 127.0.0.1:21116 --password-stdin --control --clipboard --seconds 10
```

- `--codec h264|mjpeg` 选择编码格式（两端必须一致）。Windows 默认 `h264`（Media Foundation 硬编优先）；
  其它平台默认 `mjpeg`（纯 Rust，无系统编解码依赖，逐帧关键帧、丢包只损单帧）。
- Linux 被控端（X11 GetImage 采集 + MJPEG）代码已就绪；Linux 原生 X11 输入注入与剪贴板测试已通过，完整跨机远控链路仍未验证。
- `--fps` 支持 1–120；`--bitrate` 是用户上限。连接建立后，接收端每 500ms 发送实际视频流量反馈，Agent 结合 RTT/丢包调整分辨率、帧率和码率；调整失败会回滚编码器。
- 安全默认：Agent 只监听 `127.0.0.1`，输入注入、剪贴板和文件接收均需显式启用；对外监听时必须显式指定 `--host 0.0.0.0`。口令没有默认值，长度须为 16–1024 字节。

实测（本机 2800×1840 桌面，6 秒）：398 个视频 DATAGRAM / 401 KiB、60 帧重组、24 帧解码、0 解析失败、RTT 0.571 ms；首帧转 PNG 后是真实桌面画面。详见 [design/014-real-pipeline-verified.md](design/014-real-pipeline-verified.md)。

输入与剪贴板链路（本机 8 秒实测）：合成 16 次鼠标移动 → agent 注入 16/16（忽略 0、失败 0），view 侧 `控制输入：鼠标移动 DATAGRAM = 16`；剪贴板文本双向闭环各 1 次（清单 / 拉取 / 分块 / 确认）。当前 Agent 侧需显式 `--allow-input` / `--allow-clipboard`，view 侧用 `--control` / `--clipboard` 开启；会话开始前的本地剪贴板内容不会自动上传。详见 [design/021-clipboard-sync.md](design/021-clipboard-sync.md)、[design/022-relay-client.md](design/022-relay-client.md)。

### Linux 无物理显示器虚拟显示

```bash
# Agent 创建两块隔离 Xvfb，采集第 2 块的 640×480 画面；退出时销毁
printf '%s\n' "$DFY_PASSWORD" | cargo run -p dfy-cli -- agent \
  --port 21116 --password-stdin --codec mjpeg \
  --virtual-displays 2 --virtual-monitor 1 \
  --virtual-width 640 --virtual-height 480 --virtual-refresh 60 \
  --allow-input

# 另一个终端查看/调整/销毁同一用户的受管显示
cargo run -p dfy-cli -- vdisplay list
cargo run -p dfy-cli -- vdisplay update --id 1 --virtual-width 1280 --virtual-height 720
cargo run -p dfy-cli -- vdisplay destroy --id 1
```

Linux 后端为每块显示启动独立 Xvfb，并验证 PID、启动时间和命令行所有权；采集与输入只连接
选中的 `:N` 端点，不改进程的 `DISPLAY`，也不改变物理桌面。Agent 在接收新连接前检查显示
健康；活动采集失败时会重建显示、重连采集并请求恢复后的首帧为关键帧，输入后端在连接失效后
按共享的最新端点重连。进程退出时按创建顺序的逆序销毁。`--virtual-refresh` / `--virtual-dpi`
当前只保存元数据，不能动态改变 Xvfb 刷新率或 DPI；放大几何可能重建 Xvfb，从而断开该显示上的
X11 客户端。该后端不是 DRM/Wayland 扩展显示器；Windows IddCx、macOS 和 Android 独立多屏仍未完成。
安装包需包含 `xvfb`、`xdpyinfo`、`xrandr`，见[部署说明](deploy/README.md)。

### 查看器窗口（`--ui`）

```bash
# 被控端（另一台机器 / 另一块屏）
cargo run --release -p dfy-cli -- agent --port 21116 --password-stdin --allow-input --fps 60 --bitrate 12000

# 主控端：开窗口看画面，鼠标键盘直接回传
cargo run --release -p dfy-cli -- view --addr 127.0.0.1:21116 --password-stdin --ui --control --seconds 60
```

窗口内 F11 全屏、Esc 关闭、`Ctrl+Alt+Shift+Q` 切换是否把输入发给远端。
同一台机器上自测时，窗口里会看到「窗口套窗口」——这是自捕获递归（远端画面本身就包含这个窗口），
不是渲染缺陷；要看真实效果请用两台机器，或把查看器窗口最小化后看 `--save-first` 落盘的帧。

## 项目状态（2026-11）

| 能力 | 状态 |
| --- | --- |
| DFYP/1 协议规范 + Rust workspace | ✅ v1 草案 |
| QUIC + TLS1.3 + DATAGRAM 传输 | ✅ 自测通过（ALPN `dfyp/1`，两端 SPKI 绑定） |
| Windows 采集（DXGI）→ H.264（Media Foundation）→ 串流 → 解码 | ✅ 真实桌面实测 |
| 键鼠注入（SendInput）+ 本地低级钩子采集 + 快捷键停控 | ✅ 16/16 注入实测 |
| 剪贴板双向（拉模式，文本/HTML/PNG/文件列表） | ✅ 实机 + QUIC 端到端测试 |
| 零知识中继（UDP tag 配对 / TCP 降级 / metrics） | ✅ 实机验证（视频 + 24.55 KiB 剪贴板） |
| 一键部署脚本 + Docker Compose | ✅ |
| P2P 打洞（STUN/ICE-lite） | 🚧 已接通 CLI（回环打洞 + QUIC 握手实测通过；视频端到端待真实桌面复核） |
| 会话恢复（重连/安全路径迁移） | 🚧 auth v2每次新路径认证、签名PREPARE/COMMIT、权限只减、epoch/token轮换和共享采集已通过loopback；真实P2P↔Relay自动切换未验收 |
| 上屏 UI（winit 窗口） | ✅ `view --ui [--fullscreen]`：帧渲染、键鼠/滚轮回传、与本地捕获互斥（同机自测会看到 Droste 递归假象，见 design/026 §7.2） |
| 跨平台软件编码（MJPEG 兜底） | ✅ 纯 Rust，编解码往返单测通过 |
| Linux 采集/输入/剪贴板（X11） | 🚧 Xvfb后端测试已执行；跨机与真实桌面应用未验收；Wayland不支持 |
| 文件传输（控制端 → Agent） | ✅ 实机 3 MiB SHA-256 一致；目录/续传/策略已实现 |
| Linux无头虚拟显示（隔离Xvfb） | 🚧 创建/多屏/选定端点采集与输入/调整/销毁后端测试通过；Release和真实DEB已验Agent→View及退出清理，活动杀Xvfb后188/188解码、0失败，输入换端点真实重连通过；非OS扩展显示器，网络输入/跨机/长稳未验收 |
| Windows虚拟显示（IddCx） | 🚧 用户态IOCTL与驱动骨架；驱动像素路径、签名、DXGI接线未完成 |
| macOS / Android | 🚧 Android Kotlin/JNI双角色已编译并有模拟器单屏镜像；macOS原生双角色与两平台独立多屏未完成 |
| Flutter 四端 UI | ⏳ 规划中 |

- 2026-11-25：文件传输接入 CLI（`--recv-dir` / `--send-file`），实机 3 MiB 校验通过；采集线程跨连接复用（重连/迁移不重建采集器）。
- 2026-11-25：会话恢复 v0.1（协议 tag 22/23 + agent 常驻采集线程 + 客户端确认后收帧），e2e 重连续帧测试通过（design/027）。
- 2026-11-25：上屏 UI 接入 CLI（`view --ui [--fullscreen]`）：远端帧渲染、键盘/鼠标/滚轮回传、与本地捕获互斥。同机自测时窗口里会出现「窗口套窗口」，已确认为自捕获递归（agent 采集含 view 窗口的整块桌面），非渲染缺陷；合成帧对照与像素级单测见 `design/026` §7.2。

## 路线图

- **M0** 协议规范 v1 + Rust 骨架 + QUIC 端到端自测 ✅
- **M1** Windows↔Windows MVP：采集 → H.264 → 串流 → 解码；键鼠注入；剪贴板；文件传输；查看器窗口 ✅（v0.1.0）
- **M2** NAT 打洞 + relay + 配对 + 安全加固：relay/一键部署/打洞 CLI 已可用；路径质量切换与会话迁移 v0.2 进行中 🚧
- **M3** Linux 被控/主控与 Android 双角色基础已落地；Wayland、macOS原生双角色和真机验收进行中 🚧
- **M4** Linux隔离Xvfb虚拟显示已接线；Windows/macOS/Android独立虚拟屏、音频、多屏与性能打磨进行中 🚧
- **M5** 公测发布、签名与公证、文档与协议 v1 定稿

## 图标

全端统一使用用户提供的原始图标（深蓝圆角底/薄荷绿显示器/橙鱼/奶油色指针）：
`assets/icon-source.png` 为源，`python packaging/icons/generate_icons.py` 使用 Lanczos 抗锯齿缩放，生成
Windows ICO、macOS ICNS、Linux PNG+desktop、Android mipmap，接线见
`notes/icons-all-platforms-20261127.md`。

## 许可证

- 代码：**PolyForm Noncommercial License 1.0.0**（见 `LICENSE`）。
  这是 **source-available（源码可见、仅限非商用）** 许可证，**不是 OSI 认定的开源许可证**；
  任何商业使用、商用分发或闭源衍生都不在授权范围内。
- 协议规范文本：Apache-2.0 / CC-BY-4.0（见 `spec/`），这部分与代码许可证独立。

> 用户于 2026-11-27 明确选择“非商用源码可见”，因此原需求中的“完全开源”与本许可证互斥；
> 详见 `notes/LICENSE-DECISION-20261127.md`。

## 更新日志

见 [CHANGELOG.md](CHANGELOG.md)。

## 参与

见 `CONTRIBUTING.md`。安全问题请按 `SECURITY.md` 私下报告，勿公开 issue。

---

*本项目的早期设计参考了开源社区（RustDesk、Sunshine、SudoVDA、scrcpy 等）的**架构思想**；所有代码与协议均为独立实现。*