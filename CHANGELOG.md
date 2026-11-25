# 更新日志（Changelog）

本项目为「大肥鱼远程」（Dafeiyu Remote），遵循 [语义化版本](https://semver.org/lang/zh-CN/)。
协议版本见 `spec/DFYP-v1.md`。

## [v0.1.0] - 2026-11-25

首个可用里程碑：Windows 端到端远控（采集/编码/串流/输入/剪贴板/文件），
零知识中继、UDP 打洞、一键部署与跨平台骨架。

### 新增
- **协议 DFYP/1**（`crates/dfy-proto`）：控制消息（Hello/Challenge/Auth/Welcome/Bye/Reject/Ping/Pong）、
  视频 DATAGRAM 分片头、输入事件、剪贴板 tag 13–16、文件传输 tag 17–21、会话恢复 tag 22–23；
  `spec/DFYP-v1.md` 完整规范（通道表、权限位、错误码、会话恢复、文件传输）。
- **传输层**（`crates/dfy-transport`）：quinn QUIC + TLS1.3 + Ed25519 自签证书 SPKI 绑定、
  双向控制流、DATAGRAM 视频通道、心跳/RTT、`agent_endpoint_on_socket`/`connect_on_socket`
  支持任意已打洞 socket。
- **采集**（`crates/dfy-capture`）：Windows DXGI Desktop Duplication（脏矩形、DPI 感知、多显示器）；
  Linux X11 `GetImage` + RandR 1.5/1.2/Xinerama 枚举（交叉编译通过，未实机验证）。
- **编解码**（`crates/dfy-codec`）：Windows Media Foundation H.264 硬件编码/解码；
  纯 Rust MJPEG 跨平台兜底（`Codec::Mjpeg`）；NV12↔BGRA 转换。
- **会话编排**（`crates/dfy-session`）：采集线程（限帧、丢帧策略、关键帧请求）、
  视频分片重组（乱序/丢包容错、内存上限）、agent/client 会话循环、`--fps/--bitrate` 自适应入口。
- **输入**（`crates/dfy-input`）：118 键 HID 映射、快捷键四态判定、Windows SendInput 注入
  （扫描码/扩展键/Unicode/绝对相对鼠标/滚轮/五键）、防卡键释放。
- **剪贴板**（`crates/dfy-clipboard`）：Windows 剪贴板后端（文本/HTML/PNG/DIB/文件列表/RTF）、
  轮询+事件监听、自激抑制；拉模式同步状态机（清单→按需拉取→分块）。
- **文件传输**（`crates/dfy-files`）：Offer/Accept(resume)/Chunk/Ack/Result、
  目录递归、断点续传、`OVERWRITE/SKIP/RENAME` 策略、BLAKE3 校验、`.dfy-part` 原子落盘。
- **中继**（`crates/dfy-relay`）：零知识 UDP tag 配对转发（不解析 QUIC/不终结 TLS）、
  HMAC 鉴权、限速、TCP 21118 降级、Prometheus/healthz、优雅关闭；
  `deploy/install-relay.sh` 一键部署、Docker Compose、systemd 单元。
- **P2P**（`crates/dfy-p2p`）：STUN 探测（RFC 5389 Binding，XOR-MAPPED/CHANGE-REQUEST）、
  候选枚举与优先级、UDP 打洞（cookie 校验、重试、NAT 类型提示）。
- **查看器 UI**（`crates/dfy-ui`）：winit 0.30 + softbuffer 窗口、等比缩放/信件框、
  键鼠/滚轮事件捕获与远端坐标映射、F11/Esc 快捷键、渲染统计标题。
- **CLI**（`crates/dfy-cli`）：`selftest`、`genkey`、`agent`、`view`
  （`--addr/--relay/--p2p/--codec/--ui/--control/--clipboard/--send-file/--recv-dir` 等）。
- **虚拟屏骨架**（`crates/dfy-vdisplay`）：IddCx 用户态 IOCTL 层、协议结构体、
  INF/驱动骨架与设计文档（待 WDK 编译与签名）。

### 验证
- Windows 真实桌面串流：H.264 采集→QUIC→解码，首帧 2800×1840 BGRA 落盘肉眼确认。
- 输入闭环：控制端低级钩子 → DATAGRAM/控制流 → agent SendInput 注入 16/16。
- 剪贴板双向：文本与 PNG 图片均经真实 QUIC 会话闭环（含经中继路径）。
- 文件传输：3 MiB 二进制 + 中文文本，落盘 SHA-256 与源一致，同会话视频不阻塞。
- 中继：视频 359 DATAGRAM/60 帧 + 剪贴板 24.55 KiB 经中继闭环，RTT 0.4–0.5 ms。
- 打洞：同一交换机回环下 172.18.0.1 打洞成功并完成 QUIC 握手（不支持的网段会回退中继）。
- 会话恢复：重连后 `SessionResume/ResumeOk` 续帧，e2e 测试通过。
- 质量门：`cargo fmt/clippy -D warnings/test --workspace` 全绿；GitHub Actions
  ubuntu / windows / macos 三平台构建。

### 已知限制
- 同一台机器自测查看器会看到「窗口套窗口」（自捕获递归，非缺陷）。
- P2P 打洞的视频端到端尚未在真实桌面复核（握手/RTT 已验证）。
- Linux 采集仅交叉编译通过，未在 Linux 桌面实机验证；macOS 采集/编码后端待实现。
- 虚拟屏驱动未编译/未签名；文件续传未跨重连；音频通道未实现。

[Unreleased]: https://github.com/kanghelyu/dafeiyu-remote/compare/v0.1.0...HEAD
[v0.1.0]: https://github.com/kanghelyu/dafeiyu-remote/releases/tag/v0.1.0