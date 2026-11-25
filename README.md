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

**真实屏幕串流**（Windows：DXGI 采集 + Media Foundation H.264）：

```bash
# 被控端：采集本机屏幕并推流（Ctrl+C 退出）
cargo run -p dfy-cli -- agent --port 21116 --password test --monitor 0 --fps 30 --bitrate 8000

# 主控端：连接、解码、打印统计，可选把首帧存成裸 BGRA
cargo run -p dfy-cli -- view --addr 127.0.0.1:21116 --password test --seconds 10 --save-first first-frame.bgra

# 再加控制输入（Windows 低级钩子；Ctrl+Alt+Shift+Q 停止）与双向剪贴板
cargo run -p dfy-cli -- view --addr 127.0.0.1:21116 --control --clipboard --seconds 10
```

实测（本机 2800×1840 桌面，6 秒）：398 个视频 DATAGRAM / 401 KiB、60 帧重组、24 帧解码、0 解析失败、RTT 0.571 ms；首帧转 PNG 后是真实桌面画面。详见 [design/014-real-pipeline-verified.md](design/014-real-pipeline-verified.md)。

输入与剪贴板链路（本机 8 秒实测）：合成 16 次鼠标移动 → agent 注入 16/16（忽略 0、失败 0），view 侧 `控制输入：鼠标移动 DATAGRAM = 16`；剪贴板文本双向闭环各 1 次（清单 / 拉取 / 分块 / 确认）。agent 侧默认开启剪贴板（`--no-clipboard` 关闭），view 侧用 `--clipboard` 开启。详见 [design/021-clipboard-sync.md](design/021-clipboard-sync.md)、[design/022-relay-client.md](design/022-relay-client.md)。

## 路线图

- **M0** 协议规范 v1 + Rust 骨架 + QUIC 端到端自测（进行中）
- **M1** Windows↔Windows MVP：屏幕采集 → H.264/AV1 编码 → 串流 → 解码渲染；键鼠注入；剪贴板文本；文件传输基础版
- **M2** NAT 打洞 + relay + 账号/设备配对 + 安全加固
- **M3** macOS / Linux 被控；Android 主控
- **M4** 虚拟屏（IddCx / CGVirtualDisplay / evdi）、音频、多屏、性能打磨
- **M5** 公测发布、签名与公证、文档与协议 v1 定稿

## 许可证

- 代码：**AGPL-3.0-or-later**（见 `LICENSE`）
- 协议规范文本：Apache-2.0 / CC-BY-4.0（见 `spec/`）

## 参与

见 `CONTRIBUTING.md`。安全问题请按 `SECURITY.md` 私下报告，勿公开 issue。

---

*本项目的早期设计参考了开源社区（RustDesk、Sunshine、SudoVDA、scrcpy 等）的**架构思想**；所有代码与协议均为独立实现。*