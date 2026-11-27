# 大肥鱼远程（Dafeiyu Remote）— 产品需求与范围（v0.1 草案）

> 状态：草案（基于 UU远程 功能面逆向 + 用户要求）。待 uu-app / oss-arch 报告回来后补全与定稿。

## 1. 目标
做一个**开源、跨平台、P2P 优先**的远程控制软件，覆盖 Windows / macOS / Linux / Android 四端，功能面对标 UU远程 的核心能力，但实现全部原创（参考开源底座思路，不复制原程序代码/资源/私有常量）。

## 2. 用户与场景
1. **个人远程办公**：家里控制公司电脑（文件、终端、剪贴板）。
2. **游戏/低延迟场景**（UU 的主场）：高帧率（90/144fps）、低延迟编码、虚拟显示器、手柄。
3. **远程协助**：临时协助码，让家人/同事临时接管本机（无需装客户端？→ 浏览器/轻客户端接入可选）。
4. **无人值守/运维**：开机自启、Wake-on-LAN、端口映射、远程终端 CLI。
5. **多端互控**：手机看/控 PC、PC 控手机（UU 有 remote-Window/Mac/Android/iOS 路由键，但 Android 端主机能力需我们自研）。

## 3. 功能矩阵（P0=首发必做 / P1=第二里程碑 / P2=后续）
### 3.1 连接与穿透
- P0：设备注册/登录/设备列表/在线状态；P2P 打洞（UDP）+ 中继回退；6 位/9 位协助码临时连接；连接加密（端到端，Noise/TLS1.3 风格）。
- P1：QUIC 或私有可靠 UDP 传输（丢包重传、拥塞控制、FEC）；多路径（同时走 LAN/WAN）；IPv6；UPnP/NAT-PMP 端口映射。
- P2：中继集群自托管、ICE/STUN/TURN 标准兼容、Web 客户端（WASM）。

### 3.2 视频
- P0：H.264 硬件编码（NVENC/QSV/AMF/VideoToolbox/VAAPI）+ 软件回退；动态码率/帧率；单屏采集；低延迟（<100ms LAN）。
- P1：H.265/AV1；多显示器切换/拼接；虚拟显示器（Win IDD / mac CGVirtualDisplay / Linux evdi）；super_screen（超大虚拟画布 + 视口缩放/漫游）；质量档位（清晰/流畅/自定义码率）；色度 4:2:0/4:4:4（文字清晰）；HDR（P2）。
- P2：隐私屏（远端黑屏但本地继续显示）、屏幕水印、标注/画笔（远端的标注同步）。

### 3.3 输入与快捷键
- P0：键鼠注入（Windows SendInput/驱动级；mac CGEvent；Linux uinput；Android Accessibility/ADB 可选）；绝对/相对鼠标；多屏映射；手柄（ViGEmBus 在 Windows，其他平台 SDL 虚拟手柄）。
- P1：跨系统键位映射（mac Command ↔ Win Ctrl/Alt/Win 三态策略）；快捷键透传 vs 本机拦截（可配置）；输入诊断工具；输入延迟统计。
- P2：输入法同步、触摸手势映射（手机控 PC 的触控板模式）、虚拟鼠标/智能鼠标（UU: virtual_mouse/smart_mouse/simulate_mouse）。

### 3.4 剪贴板与文件
- P0：文本剪贴板双向同步（按需/自动开关）；文件传输（拖拽、进度、断点续传）。
- P1：图片/富文本格式协商（多格式列表 + 按需拉取，对应 UU remote_clipboard_v1..v5）；文件剪贴板（复制粘贴文件，含目录）；智能粘贴规则。
- P2：大文件 FTP 通道（UU file_transfer_ftp/ftp2）、目录打包压缩传输。

### 3.5 音频
- P0：远端系统声音回传（虚拟声卡或 loopback）。
- P1：麦克风双向（对讲）、音量/静音控制；虚拟声卡驱动（Windows nevaudio 类；mac CoreAudio；Linux PipeWire null-sink）。
- P2：音频编码 Opus 自适应、回声消除。

### 3.6 配套功能
- P0：文件管理器/远程终端（conpty/pty 转发）、远程重启/关机/锁屏、开机自启（服务/守护进程）。
- P1：远程命令行 CLI（`dafeiyu-cli` + 本地 HTTP API：设备列表/连接/终端/状态/JSON 输出）；Wake-on-LAN；端口映射管理；多会话标签页；会话录制（P2）。
- P2：云电脑形态（可选）、插件体系（UU 有 plugin 机制）、消息中心/反馈（Web 侧）。

## 4. 非目标（首版不做）
- 不做账号体系对接网易/OpenAI 等第三方；自建账号（邮箱/匿名设备码）。
- 不做远程桌面协议的"标准兼容"（不实现 RDP/VNC 兼容层，P2 可加 VNC 服务端模式）。
- 不复制 UU 私有协议（gv_pb）与 UI 资源；只参考其架构与交互。

## 5. 里程碑（草案）
- M0 调研收口：四份 research + 需求定稿（1 周内）
- M1 骨架跑通：Rust workspace（core/proto/transport/relay） + Tauri 壳 + 本地双端同机 loopback 串流（不编码，原始帧）
- M2 MVP-PC↔PC：Win↔Win 采集→H.264→P2P→渲染→键鼠→剪贴板→文件；自建信令（最小可用）
- M3 跨平台：Linux 被控（X11/PipeWire/evdi）、mac 被控（CGDisplayStream/CGVirtualDisplay/AX）、Android 客户端
- M4 虚拟屏/多屏/快捷键映射/音频/终端/CLI/协助码
- M5 打磨：性能、弱网、安全审计、文档、GitHub 发布

## 6. 安全模型（必须从 M1 就设计）
- 设备身份：每设备生成 Ed25519 密钥对，device_id = base32(公钥指纹前 16 字符)（可参考 UU 的 16 位 ID 形态）。
- 配对：协助码 = 短期随机 9 位数字；持久配对 = 双方确认后存公钥。
- 会话：临时 X25519 密钥交换 + AEAD（ChaCha20-Poly1305/AES-GCM），端到端，中继/信令不可解密。
- 权限：允许控制/仅查看/仅文件/仅终端等分级（UU 有 controlled_ability_map/controller_ability_map）。
- 隐私：隐私屏、剪贴板开关、连接提示、水印、审计日志（本地加密）。

## 7. 开源与授权（2026-11-27 用户已拍板）
- 最终采用 **PolyForm Noncommercial License 1.0.0**：禁止商用、源码可见，属于 source-available 而非 OSI 开源。
- 原需求“完全开源”与“禁止商用”法律上互斥；本决策以用户后续明确选择为准，原需求原文保留不改。
- 仓库结构（计划）：`crates/{proto,transport,relay,core-server,core-client,capture-*,input-*,display-*,clipboard,audio}`、`apps/{desktop-tauri,cli,android}`、`docs/`、`scripts/`。