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
| `crates/` | Rust workspace：协议、传输、NAT、采集、编解码、输入、剪贴板、文件、虚拟屏、会话、CLI |
| `spec/` | 自研协议规范（DFYP/1） |
| `design/` | 产品与架构设计文档 |
| `apps/` | 四端 UI（Flutter，脚手架规划中） |
| `research/` | 逆向/调研笔记（**仅本地保留，不随仓库发布**） |

## 文件传输（控制端 → Agent）

控制端把本地文件/目录发给 Agent，Agent 落到指定目录；协议见 `design/024-file-transfer.md`，
v1 复用 control 流（256 KiB/块，按 16 MiB 或 64 块回 Ack，BLAKE3 校验，原子落盘 `.dfy-part` → rename）。

```bash
# Agent：开启接收目录（不存在会自动创建）
cargo run --release -p dfy-cli -- agent --port 21116 --recv-dir ./incoming

# 控制端：发送文件（可多次 --send-file，目录会被展开递归发送）
cargo run --release -p dfy-cli -- view --addr 127.0.0.1:21116 --seconds 15 \
  --send-file ./report.pdf --send-file ./screenshots/
```

实测（Windows 本机回环，直连）：3 MiB 随机二进制 + 54 B 文本 → `文件：offer 2，块 13（3.00 MiB），确认 2，结果 2，完成 2，失败 0`，
落盘两个文件 SHA-256 与源文件一致。

## 经中继连接（防火墙友好的兜底路径）

当两端都在 NAT 后面、UDP 打洞不可行时，用一台有公网 IP 的机器做中继。
中继是**零知识**的：它只按 tag 把两个 UDP 端点的包互相转发，不解析 QUIC、不终结 TLS，
因此端到端加密不受影响。

```powershell
# 1) 在中继机生成配置并启动（Linux/Windows 均可）
dfy-relay config init --out relay.toml
dfy-relay serve --config relay.toml

# 2) 被控端：指定中继地址与 relay_key（tag 自动按口令派生）
dfy agent --relay relay.example.com:21117 --relay-key 'base64:<relay_key>'

# 3) 主控端：同样的中继参数
dfy view --relay relay.example.com:21117 --relay-key 'base64:<relay_key>' --clipboard
```

两端口令一致时 tag 自动相同；如需固定 tag，用 `--relay-tag <32-hex>`。

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
cargo run -p dfy-cli -- demo-agent --port 21116 --password test
cargo run -p dfy-cli -- demo-client --addr 127.0.0.1:21116 --password test
```

**真实屏幕串流**（Windows：DXGI 采集 + Media Foundation H.264；其它平台：MJPEG 兜底）：

```bash
# 被控端：采集本机屏幕并推流（Ctrl+C 退出）
cargo run -p dfy-cli -- agent --port 21116 --password test --monitor 0 --fps 30 --bitrate 8000

# 主控端：连接、解码、打印统计，可选把首帧存成裸 BGRA
cargo run -p dfy-cli -- view --addr 127.0.0.1:21116 --password test --seconds 10 --save-first first-frame.bgra

# 再加控制输入（Windows 低级钩子；Ctrl+Alt+Shift+Q 停止）与双向剪贴板
cargo run -p dfy-cli -- view --addr 127.0.0.1:21116 --control --clipboard --seconds 10
```

- `--codec h264|mjpeg` 选择编码格式（两端必须一致）。Windows 默认 `h264`（Media Foundation 硬编优先）；
  其它平台默认 `mjpeg`（纯 Rust，无系统编解码依赖，逐帧关键帧、丢包只损单帧）。
- Linux 被控端（X11 GetImage 采集 + MJPEG）代码已就绪，交叉编译通过；**尚未在真实 Linux 上实机验证**。

实测（本机 2800×1840 桌面，6 秒）：398 个视频 DATAGRAM / 401 KiB、60 帧重组、24 帧解码、0 解析失败、RTT 0.571 ms；首帧转 PNG 后是真实桌面画面。详见 [design/014-real-pipeline-verified.md](design/014-real-pipeline-verified.md)。

输入与剪贴板链路（本机 8 秒实测）：合成 16 次鼠标移动 → agent 注入 16/16（忽略 0、失败 0），view 侧 `控制输入：鼠标移动 DATAGRAM = 16`；剪贴板文本双向闭环各 1 次（清单 / 拉取 / 分块 / 确认）。agent 侧默认开启剪贴板（`--no-clipboard` 关闭），view 侧用 `--clipboard` 开启。详见 [design/021-clipboard-sync.md](design/021-clipboard-sync.md)、[design/022-relay-client.md](design/022-relay-client.md)。

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
| 上屏 UI（winit 窗口） | 🚧 crate 已落盘，待验收与接入 |
| 跨平台软件编码（MJPEG 兜底） | ✅ 纯 Rust，编解码往返单测通过 |
| Linux 采集（X11 GetImage + RandR 枚举） | 🚧 交叉编译通过，未实机验证 |
| 文件传输 | 🚧 开发中（协议消息已落地） |
| 虚拟屏（IddCx 驱动骨架 + 用户态 IOCTL） | 🚧 骨架完成，待 WDK 编译与签名 |
| macOS / Android | 🚧 逆向已完成，待实现 |
| Flutter 四端 UI | ⏳ 规划中 |

- 2026-11-25：文件传输接入 CLI（`--recv-dir` / `--send-file`），实机 3 MiB 校验通过；采集线程跨连接复用（重连/迁移不重建采集器）。

## 路线图

- **M0** 协议规范 v1 + Rust 骨架 + QUIC 端到端自测 ✅
- **M1** Windows↔Windows MVP：采集 → H.264 → 串流 → 解码；键鼠注入；剪贴板；文件传输（进行中；串流/输入/剪贴板已可用）
- **M2** NAT 打洞 + relay + 配对 + 安全加固（relay 路径已可用，P2P 打洞进行中）
- **M3** macOS / Linux 被控；Android 主控（逆向完成）
- **M4** 虚拟屏（IddCx / CGVirtualDisplay / evdi）、音频、多屏、性能打磨
- **M5** 公测发布、签名与公证、文档与协议 v1 定稿

## 许可证

- 代码：**AGPL-3.0-or-later**（见 `LICENSE`）
- 协议规范文本：Apache-2.0 / CC-BY-4.0（见 `spec/`）

## 参与

见 `CONTRIBUTING.md`。安全问题请按 `SECURITY.md` 私下报告，勿公开 issue。

---

*本项目的早期设计参考了开源社区（RustDesk、Sunshine、SudoVDA、scrcpy 等）的**架构思想**；所有代码与协议均为独立实现。*