# 交接文档（HANDOFF）— 大肥鱼远程 / Dafeiyu Remote

> 面向接手的下一个模型或工程师。最后更新：2026-11-27（发布后补写）。
>
> **一句话状态**：源码已发布（`v0.1.0-source-available`），许可证为非商用 source-available；
> 核心链路与部分平台能力有真实证据，但**不是所有端产品级完成**，不得对外宣称完成。

---

## 0. 发布与仓库状态（先看这里）

| 项 | 值 |
| --- | --- |
| 本地仓库 | `D:\agent working\dafeiyu` |
| 远端 | `https://github.com/kanghelyu/dafeiyu-remote.git` |
| 分支 | `main` |
| 发布提交 | `7f027c82aba0680e8e29593d8218156b1d4a0b76` |
| 发布标签 | `v0.1.0-source-available`（同一提交） |
| Release | `https://github.com/kanghelyu/dafeiyu-remote/releases/tag/v0.1.0-source-available` |
| Release 标题 | `v0.1.0 source-available snapshot (NOT product-grade)` |
| 许可证 | `PolyForm Noncommercial License 1.0.0`，source-available，禁止商用，**不是 OSI 开源** |
| 发布时工作区 | 干净（commit + push 完成） |
| 本文件状态 | 发布后新增/重写，**尚未提交**；需要再次 push 时由接手者决定 |

不要因为“已发布”就假设产品级完成。Release 正文与 `D:\agent working\dafeiyu\docs\STATUS.md`、`D:\agent working\dafeiyu\docs\FEATURE-MATRIX.md`
明确列出缺口。

---

## 1. 用户目标、授权边界与不可更改项

### 1.1 原始需求

- 不可修改：`D:\agent working\dafeiyu\docs\requirements\000-original-user-prompt.md`
- 校验值：SHA-256 `3db9a60032c0e801593be04f6094b376c97794521fa855d413acbade971cd4e3`
- 规模：825 行 / 583 非空行 / 242 非空块 / 28 个需求模块
- 校验命令：`python "D:\agent working\dafeiyu\docs\requirements\split-and-verify.py" --check`（每次改动文档后都跑）
- 原始目标：开源、跨平台、自建中继集群、高性能远控；Windows/Linux/macOS/Android 四端双角色、
  独立安装包、真实虚拟屏、自定义分辨率/码率/120fps、弱网自适应、UU 等价能力。

### 1.2 许可证决策（覆盖原需求中的“开源”）

- 用户先要求“开源协议不允许商用”；已澄清 OSI 开源许可证不得限制商用，二者互斥。
- 用户选择：**改成非商用源码可见**。
- 因此 LICENSE 使用 PolyForm Noncommercial 1.0.0；全仓 SPDX 为
  `LicenseRef-PolyForm-Noncommercial-1.0.0`；`cargo metadata` 验证所有包一致。
- 决策记录：`D:\agent working\dafeiyu\notes\LICENSE-DECISION-20261127.md`。原需求文件未修改。
- 后果：不得声称 OSI 开源；不得授权商业使用、商用分发或闭源衍生。

### 1.3 发布授权

- 用户在 2026-11-27 明确说“发布”，因此完成了一次 commit + push + GitHub Release。
- 之后的新改动（包括本文件）**没有**新的 push 授权；继续改动后不要擅自 push。

---

## 2. 代码地图（按 crate / 目录）

| 路径 | 职责 | 关键文件 |
| --- | --- | --- |
| `D:\agent working\dafeiyu\crates\dfy-proto` | DFYP/1 线格式：手写 prost Envelope、帧长度前缀、视频/输入/音频 DATAGRAM | `src/msg.rs`, `src/datagram.rs`, `src/consts.rs` |
| `D:\agent working\dafeiyu\crates\dfy-transport` | quinn/TLS1.3、握手 v2、签名会话迁移、心跳、设备身份 | `src/handshake.rs`, `src/resume.rs`, `src/auth.rs` |
| `D:\agent working\dafeiyu\crates\dfy-session` | Agent/View 会话编排、采集循环、自适应、迁移上下文、replay 状态机 | `src/agent.rs`, `src/client.rs`, `src/capture_loop.rs`, `src/migration.rs`, `src/adaptation.rs` |
| `D:\agent working\dafeiyu\crates\dfy-capture` | Windows WGC/DXGI、Linux X11、可恢复虚拟显示采集 | `src/wgc.rs`, `src/linux.rs`, `src/lib.rs` |
| `D:\agent working\dafeiyu\crates\dfy-codec` | Windows MF H.264/MJPEG 编解码、低延迟修复、软件回退 | `src/mf.rs`, `src/jpeg.rs` |
| `D:\agent working\dafeiyu\crates\dfy-input` | HID/VK/扫描码映射、Windows SendInput、Linux XTest/XInput2、按下状态释放 | `src/win.rs`, `src/x11.rs`, `src/event.rs` |
| `D:\agent working\dafeiyu\crates\dfy-clipboard` | 拉式剪贴板状态机、Win32/X11 后端、HTML/DIB/PNG/文件列表 | `src/sync.rs`, `src/win.rs`, `src/linux.rs` |
| `D:\agent working\dafeiyu\crates\dfy-files` | 文件传输 offer/chunk/ack/result、BLAKE3、原子落盘 | `src/sender.rs`, `src/receiver.rs` |
| `D:\agent working\dafeiyu\crates\dfy-vdisplay` | 虚拟显示统一接口、Linux Xvfb 后端、Windows IDD 用户态/驱动骨架 | `src/lib.rs`, `src/linux_xvfb.rs`, `src/win_driver.rs`, `driver/` |
| `D:\agent working\dafeiyu\crates\dfy-path` | 路径质量状态机：RTT/丢包/stall 采样、SwitchTo/RequestProbe、回滚/退避 | `src/machine.rs`, `src/adaptation.rs` |
| `D:\agent working\dafeiyu\crates\dfy-relay` | 独立 UDP/TCP 中继、tag/role 配对、限速、指标、admin | `src/relay.rs`, `src/udp.rs`, `src/state.rs` |
| `D:\agent working\dafeiyu\crates\dfy-relay-profiles` | 用户中继池档案、四种选择策略、健康状态、导入导出/脱敏 | `src/lib.rs` |
| `D:\agent working\dafeiyu\crates\dfy-p2p` | STUN 解析、候选、UDP 打洞桥 | `src/stun.rs`, `src/punch.rs` |
| `D:\agent working\dafeiyu\crates\dfy-audio` | cpal 设备后端、PCM 抖动缓冲、增益/静音；feature `device-backend` | `src/lib.rs` |
| `D:\agent working\dafeiyu\crates\dfy-ui` | winit+softbuffer Viewer 上屏、键位/滚轮翻译、渲染统计 | `src/app.rs`, `src/frame.rs` |
| `D:\agent working\dafeiyu\crates\dfy-manager` | 本地图形管理面板：中继/虚拟显示/Diagnostics 三页签 | `src/main.rs`, `src/draw.rs` |
| `D:\agent working\dafeiyu\crates\dfy-cli` | 所有 CLI 子命令与真实 agent/view 接线 | `src/main.rs`, `src/real.rs`, `src/input_capture.rs` |
| `D:\agent working\dafeiyu\crates\dfy-android` | Android Rust JNI 宿主 | `src/agent.rs`, `src/controller.rs`, `src/lib.rs` |
| `D:\agent working\dafeiyu\android\` | Kotlin UI/服务、MediaProjection、解码器、质量反馈 | `app/src/main/java/remote/dafeiyu/` |
| `D:\agent working\dafeiyu\packaging\` | Linux DEB/RPM、Windows Inno Setup、macOS pkg/dmg、Android 构建与图标生成 | `linux/`, `windows/`, `macos/`, `icons/generate_icons.py` |
| `D:\agent working\dafeiyu\assets\` | 统一图标源与预览 | `icon-source.svg`, `icon-512.png` |
| `D:\agent working\dafeiyu\design\` `D:\agent working\dafeiyu\spec\` `D:\agent working\dafeiyu\docs\` | 设计、协议规范、状态/证据矩阵、需求模块 | `D:\agent working\dafeiyu\docs\STATUS.md`, `D:\agent working\dafeiyu\docs\FEATURE-MATRIX.md` |
| `D:\agent working\dafeiyu\notes\` | 验收证据与交接；白名单发布，UU 逆向资料留本地 | `D:\agent working\dafeiyu\notes\HANDOFF.md` 等 |

`D:\agent working\dafeiyu\.gitignore` 关键项：`/target`、`.tmp/`、`*.exe`、`.env`、`/research/`、`/notes/*`（白名单例外）、
`device.key`、`device.pub`、`dfy-relay-profiles.toml*`、`*.log`、`D:\agent working\dafeiyu\deploy\relay.toml`、Android `jniLibs/` 与 `build/`。
**任何密钥、专有二进制、UU 解包资料都不能入库。**

---

## 3. 已验证能力（只列有证据的）

证据总入口：`D:\agent working\dafeiyu\docs\STATUS.md`（逐条命令、环境、结果、SHA-256）；
`D:\agent working\dafeiyu\docs\FEATURE-MATRIX.md`（逐项实现/证据/缺口）；
白名单 `D:\agent working\dafeiyu\notes\*.md`（详细验收记录）。

### 3.1 核心链路

- Windows WGC/DXGI → MJPEG/H.264 → QUIC → Android Viewer 真实显示；
  Windows↔Linux 同机 QEMU 双向视频、指针、Unicode 剪贴板、文件哈希。
  证据：`D:\agent working\dafeiyu\notes\crossplatform-acceptance-20261126.md`。
- Linux X11 采集 + Xvfb 虚拟屏：Agent→View 640×480 链路、像素来源验证、退出清理。
- Windows 控制端输入：5ms 光标/按键轮询兜底；合成**键钮**在本 harness 不可观察，故未验证。
- Linux 控制端输入：XInput2 raw 优先、X11 根事件降级；隔离 Xvfb 实测指针/左键/A 键/滚轮/停止热键。

### 3.2 虚拟显示

- `XvfbBackend`：每显示独立 PID/start-time/cmdline + flock 所有权，几何握手验证，故障恢复。
- Agent 5s 后台健康任务：无 Viewer 连接时杀掉受管 Xvfb，8s 内重建。
- DPI：Xvfb 启动加 `-dpi`，`xdpyinfo` 读回 120DPI；144DPI 虚拟屏上 `xmessage` 真实应用窗口令
  两帧相差 40,181 像素（非黑同数），证明真实应用上屏。
  证据：`D:\agent working\dafeiyu\notes\vdisplay-linux-app-window-20261127.md`。
- 缺口：Windows IDD 未签名装机、macOS/Android 独立无头多屏、动态刷新率、Wayland/DRM、
  虚拟屏内应用输入（外部合成鼠标在 Windows harness 不被 View 观察）。

### 3.3 中继与路径迁移

- `dfy relay-profile` CRUD/健康/选择/脱敏导入导出；真实本机 relay 往返 2.86ms，失败目标写回。
- 健康探针：随机 tag 的一对 Client/Agent socket + 8×1200B 数据面探测，写回 rtt/loss/bps；
  本机 2.31ms / 0ppm / 3.84Mbps（小突发观测值，非容量测量）。
- `--auto-path`：双端点 + `PathMonitor` + 签名 PREPARE/COMMIT + epoch/token 轮换 + 帧进度续传。
  强制双向迁移、真实拥塞自动迁移均通过（epoch=1、恢复确认、旧会话 migrated、帧连续）。
  `Action::RequestProbe` 接轻量 QUIC 探测，实测多次探测并按 5 个好样本请求回切。
- 迁移期业务状态：44B 文件 offer 哈希一致；迁移瞬间按住键/按钮重放（`replayed=1`）；
  跨 OS 强制迁移后双向 35B 剪贴板同步。
- 证据：`D:\agent working\dafeiyu\notes\relay-profiles-acceptance-20261126.md`、`D:\agent working\dafeiyu\notes\auto-path-acceptance-20261126.md`、
  `D:\agent working\dafeiyu\notes\active-path-probe-20261127.md`、`D:\agent working\dafeiyu\notes\input-replay-migration-20261127.md`、
  `D:\agent working\dafeiyu\notes\crossos-migration-clipboard-20261127.md`。

### 3.4 音频

- `--audio-out` / `--audio-in` 已接入 agent/view；16kHz/单声道/20ms s16le DATAGRAM。
- `AudioJitterBuffer`：按 seq 重排、重复/乱序计数、送出时按空隙计丢包、自适应目标延迟
  （20–200ms）、u32 截断时间戳出队回归测试。
- `GainSink`：`--volume` / `--mute` 软件增益，静音输出 0（单测验证；未测扬声器实际输出）。
- 跨 OS 直连：448 DATAGRAM / 445 帧；强制迁移新纪元：572 DATAGRAM / 569 帧 / 丢包 0 / 淘汰 0。
  先前“814 收 / 571 播”根因是旧纪元积压包误入缓冲再被 `reset_sequence()` 清空，已修复。
- 本机无输入设备，用 `--audio-test-tone` 诊断源替代麦克风；真实采集未验收。
  证据：`D:\agent working\dafeiyu\notes\audio-first-step-20261127.md`。

### 3.5 图形面板与图标

- `dfy-manager`：winit+softbuffer+ab_glyph；Relay / Displays / Diagnostics 三页签；
  T/R 测试、U 设为当前、C/D 预设虚拟屏创建/销毁、G 诊断、F5 刷新、`--tab` 直达。
- Windows 真实起窗截图与 7 单测通过；击键路径因共享桌面焦点不稳定未验收。
- 全平台统一像素风图标：用户提供图片，按图面手工重建 64×64 母版（**不是原图逐像素拷贝**），
  生成 Windows ICO / macOS ICNS / Linux PNG+desktop / Android mipmap 并接线。
- 证据：`D:\agent working\dafeiyu\notes\manager-ui-20261127.md`、`D:\agent working\dafeiyu\notes\icons-all-platforms-20261127.md`。

### 3.6 打包与发布

- Linux DEB：新增 `prerm`/`postrm` 与 `verify-deb.sh`；guest 真实跑通
  构建 → 元数据/许可/图标/desktop 校验 → `dpkg -i` → 两 ELF 帮助 → `dpkg -r` 清理。
  证据：`D:\agent working\dafeiyu\notes\deb-installer-accept-20261127.md`。
- Windows Inno Setup：脚本与图标接线；本机当前无 ISCC，无法重新构建/升级回归（旧证据见 STATUS）。
- macOS pkg/dmg：脚本存在，icns 放入包内；本机无 macOS/CD 证书，未验证。
- Android：Debug APK 曾在 API36 模拟器安装并验证非黑像素、Apply/Reset、硬件 Back；
  当前本机无 JDK，无法重新构建资源（Manifest 改用 mipmap 后只做 XML 静态校验）。

---

## 4. 环境与复现命令（重要坑）

### 4.1 Windows 主机

- Rust 工作区；`cargo` 正常，`rustfmt` / `clippy` 可用。
- 独立临时目录与 target（避免污染）：
  ```powershell
  $env:TMP='D:/agent working/dafeiyu/.tmp/cc-temp-review'
  $env:TEMP=$env:TMP
  $env:TMPDIR=$env:TMP
  $env:CARGO_TARGET_DIR='D:/agent working/dafeiyu/.tmp/cargo-target-crossplatform-20261126'
  ```
- **不要**跑宽范围 `cargo fmt --all`；只 fmt 改动过的 crate。
- Git Bash 在 `C:\Program Files\Git\bin\bash.exe`；Inno Setup 当前未安装
  （曾有过 6.7.3，现本机找不到 `ISCC.exe`）。
- Android Gradle 8.11.1 在 `~/.gradle/wrapper/dists/...`；**JDK 当前不存在**，`JAVA_HOME` 未配置。

### 4.2 截图与焦点

- `D:\agent working\dafeiyu\.tmp\screenshot.ps1` + `D:\agent working\dafeiyu\.tmp\focus_window.ps1` 曾用于面板截图；
  共享桌面焦点不稳定，`SendKeys` 可能打到其它应用，不要依赖击键做验收。
- Windows 合成 `SendInput`/`mouse_event` 的**键钮**不进入本 harness 的低级钩子/Raw Input；
  绝对移动可读，但 Python 进程与 View 进程读到的 `GetCursorPos` 坐标曾不一致
  （(29,16) vs (599,404)），导致虚拟屏内应用输入验收失败。

### 4.3 Linux guest（Debian 13 QEMU TCG）

- SSH：`ssh -F 'D:/agent working/.tmp/dfy-linux-lab/ssh_config' dfy-linux-lab`
- `sudo -n` 可用；磁盘 20G，曾满盘，已删除旧的 `dfy-validation-target-*`、`dafeiyu-recovery-*`、
  `dafeiyu-vdisplay-*` 源码树与 tar 腾出空间。
- 关键保留目录：
  - 源码：`/home/lab/work/dafeiyu-linux-controller-20261126`
  - 当前 target：`/home/lab/work/cargo-target-linux-controller-20261126`
  - 旧 release dfy-relay：`/home/lab/work/dfy-vdisplay-target-20261125/release/dfy-relay`
- QEMU hostfwd：24800/24803 曾可用（24801 被转发回 guest，慎用）；
  relay 从 guest 访问 Windows 主机用 `10.0.2.2:21117`，relay 需 bind `0.0.0.0:21117`。
- **坑**：guest 里重新链接 `dfy-relay` 偶发 `collect2: ld terminated with signal 7 [Bus error]`
  （TCG + lld）；打包验收复用已成功链接的 ELF，不把它当产品缺陷。
- ALSA 开发库缺失，`dfy-audio` 默认 feature 为空；启用 `device-backend` 只在 Windows 验证过。
- guest 未装 clippy。

### 4.4 常用验收命令

```powershell
# 完整性 / 链接 / diff
python "D:/agent working/dafeiyu/docs/requirements/split-and-verify.py" --check
python 'D:/agent working/dafeiyu/.tmp/check_docs_links.py'
git diff --check

# 定向测试
cargo test -p dfy-cli -p dfy-path -p dfy-relay-profiles -p dfy-manager -p dfy-proto -p dfy-vdisplay
cargo test -p dfy-audio --features device-backend
cargo clippy -p dfy-cli -p dfy-audio -p dfy-path -p dfy-relay-profiles -p dfy-manager -p dfy-proto -p dfy-vdisplay --all-targets -- -D warnings
```

最后全绿计数：CLI 46、manager 7、path 39、adaptation 13、machine 5、relay-profiles 4、
proto 22、vdisplay 22、audio 9（feature）、doctest 若干，全部 0 失败。

### 4.5 文件写入陷阱

Python 在 Windows 上用 `write_text()` 默认会把 `\n` 转成 `\r\n`，曾导致 `git diff --check`
报成百上千条“trailing whitespace”。批量替换源码请用 `read_bytes()/write_bytes()` 或显式 `newline="\n"`。
批量改许可证时用的脚本在 `D:\agent working\dafeiyu\.tmp\relicense.py`、`D:\agent working\dafeiyu\.tmp\relicense2.py`（未入库）。

---

## 5. 关键设计决策（避免重复踩坑）

1. **许可证**：用户后续选择覆盖原需求“开源”；非商用 source-available，不宣称 OSI 开源。
2. **路径迁移**：不是简单重连，而是 PREPARE/COMMIT 签名、权限只减、epoch/token 轮换、
   `SessionResume` 精确进度；新路径握手超时 5s→15s（QEMU NAT 下 5s 会误判）。
3. **迁移期业务状态**：剪贴板基线在会话启动时记录，迁移后刷新基线避免把“已有内容”当新变化；
   文件靠 offer/ack 进度；输入按下集合重放且注入端幂等；音频**不跨纪元拼接**，旧 seq 包丢弃。
4. **音频时钟**：DATAGRAM 的 `ts_us` 是 u32 微秒截断（约 71 分钟回绕），出队判断用本地完整
   `now_us` 与固定/自适应延迟；不要用 `try_from` 饱和到 `u32::MAX`。
5. **抖动缓冲淘汰**：不要按“新包时间戳”淘汰旧帧（突发到达会静默丢帧），只用容量上限并计入
   `evicted` 统计；tick 不追赶到未来，长停顿后从当前时刻重新起拍。
6. **虚拟显示 DPI**：Xvfb 只能启动时设 `-dpi`，运行时不可改；`refresh_hz` 只是元数据，别写成真实刷新率。
7. **图标**：用像素母版 + 最近邻放大；各端只引用生成物，`D:\agent working\dafeiyu\assets\icon-source.svg` 是源。
8. **安装脚本**：DEB 不启用/启动 relay，必须先由运维生成 relay_key；卸载脚本只 disable，
   purge 时才尝试清理空配置目录。

---

## 6. 已知失败/未验收场景（不要记成通过）

| 场景 | 结论 | 原因/证据 |
| --- | --- | --- |
| Windows 合成键盘/鼠标按钮采集 | 未验收 | 本 harness 不进入钩子/Raw Input/跨进程异步状态 |
| 虚拟屏内应用的网络输入 | 未验收 | 外部 Python 与 View 进程读到的光标坐标不一致，View 发送 0 move |
| 迁移后剪贴板早期复测失败 | 已定位并修复 | 测试拓扑错误（同机共用剪贴板却检查 Linux）+ 5s 握手超时；见对应 note |
| 音频 814 收/571 播 | 已定位并修复 | 旧纪元积压包误入缓冲、容量淘汰 + reset 清空；见音频 note |
| Windows IDD | 未完成 | 未签名/未装机，DXGI 端点未接通 |
| macOS 原生双角色/虚拟屏 | 未实现 | 只有 stub，未在本环境编译/运行 |
| Android 真机/签名上架 | 未验收 | 无 JDK/签名证书；模拟器证据不等于真机 |
| 两台物理设备/公网 NAT | 未验收 | 全部跨机证据都是同机 QEMU |
| H.265/AV1、120fps/12Mbps 长稳 | 未完成 | 仅有能力与静态边界 |
| 发布签名、SBOM、可复现构建矩阵 | 未完成 | 当前包 unsigned |
| `dfy-manager` 窗口图标 | 已完成（测试通过） | `dfy-ui`与`dfy-manager`均已接入`assets/icon-512.png`解码与`with_window_icon`（Linux单测通过，Windows沙盒桌面未弹窗复核） |

---

## 7. 平台缺口清单（按优先级）

### P0 — 产品级发布前置
- Windows IDD 驱动签名/HLK/WHQL、装机与 DXGI 端点；无头多屏。
- macOS ScreenCaptureKit + TCC + 原生输入/剪贴板 + CGVirtualDisplay + 签名公证 + 真机。
- Windows 安装包代码签名、升级覆盖回归；当前无 ISCC 环境。
- Android 签名发布包、真机、独立无头多屏、输入/文件/剪贴板互操作。

### P1 — 功能闭环
- 两台物理设备/公网 NAT/Relay 验收。
- 弱网长稳、120fps/12Mbps、硬件编解码。
- 质量统计在 GUI 全量可见（RTT/丢包/码率/FPS/分辨率/延迟/路径）。
- 设备发现/拨号/会话管理/最近连接/收藏/搜索/历史等完整主界面。
- 迁移期鼠标绝对坐标、30s 窗口边界、探测参数标定。
- 音频丢包补偿、回声消除、自适应延迟长稳标定。

### P2 — 工程化
- RPM/aarch64 构建验证、systemd 服务启停验证。
- SBOM、依赖许可证全量审计、可复现构建、CI 绿灯矩阵。
- 本地化与可访问性、主题/审美打磨。

---

## 8. 下一步建议（接手者按此顺序）

1. **先跑校验**：`split-and-verify.py --check`、`check_docs_links.py`、`git diff --check`、定向测试与 clippy。
2. **Windows 安装包**：恢复 ISCC 环境（官方 Inno Setup 6.7.x），以同一 AppId 构建 0.1.0/0.1.1
   两个版本，验证 `/VERYSILENT` 安装 → 文件替换 → 卸载 → 注册表/快捷方式清理。
3. **`dfy-manager` 窗口图标**：用 `png` crate 解码 `D:\agent working\dafeiyu\assets\icon-512.png` 生成
   `winit::window::Icon::from_rgba`，给 `WindowAttributes::with_window_icon`。
4. **Linux RPM/ARM64**：在有 `rpmbuild` 的环境验证 spec；QEMU 无法真跑 aarch64。
5. **路径探测口径**：在拥塞窗口内做 64B/1200B 对照，确认大包探测是否真能识别大包瓶颈。
6. **音频迁移间隙**：把迁移做成非阻塞任务，避免握手期间阻塞主循环导致旧纪元音频积压。
7. **平台缺口**：按 P0 清单逐项找真机/证书/SDK；没有环境就不要写“已验证”。

无论哪一步，结论都必须落到 `D:\agent working\dafeiyu\docs\STATUS.md` + `D:\agent working\dafeiyu\docs\FEATURE-MATRIX.md` + 对应 `D:\agent working\dafeiyu\notes\*.md`，
写清环境、命令、结果、SHA-256 与边界。

---

## 9. 红线（必须遵守）

- 不 commit/push 未授权的新改动；本文件发布后重写，尚未 push。
- 不提交任何密钥、设备私钥、relay 真实密钥、CI token、`lab_ed25519`、`device.key`。
- 不把 proprietary 解包资料（UU 制品、`D:\agent working\dafeiyu\research\`、`D:\agent working\dafeiyu\notes\uuyc-*` 等）发布或再分发。
- 不把 mock/loopback/同机 QEMU/编译通过写成跨平台或产品级验收。
- 不修改 `D:\agent working\dafeiyu\docs\requirements\000-original-user-prompt.md`。
- 许可证已定为非商用 source-available，任何“开源”表述都要限定为“源码可见”。
- 产品级宣称需要各端真机/签名/安装包/公网证据；没有就继续写“未完成”。

---

## 10. 附录：关键证据索引与哈希

| 主题 | 文件 | 关键 SHA-256 / 结果 |
| --- | --- | --- |
| 原始需求 | `D:\agent working\dafeiyu\docs\requirements\000-original-user-prompt.md` | `3db9a60032c0e801593be04f6094b376c97794521fa855d413acbade971cd4e3` |
| 许可证决策 | `D:\agent working\dafeiyu\notes\LICENSE-DECISION-20261127.md` | PolyForm Noncommercial 1.0.0 |
| 跨 OS 基础 | `D:\agent working\dafeiyu\notes\crossplatform-acceptance-20261126.md` | 双向视频/指针/剪贴板/文件 |
| 中继池 | `D:\agent working\dafeiyu\notes\relay-profiles-acceptance-20261126.md` | 2.86ms / 0ppm / 3.84Mbps |
| 自动迁移 | `D:\agent working\dafeiyu\notes\auto-path-acceptance-20261126.md` | epoch=1、恢复确认、真实拥塞自动触发 |
| 主动探测 | `D:\agent working\dafeiyu\notes\active-path-probe-20261127.md` | 1200B 探测、5 次、请求回切 |
| 跨 OS 剪贴板 | `D:\agent working\dafeiyu\notes\crossos-migration-clipboard-20261127.md` | 双向 35B、帧 #23/23 |
| 按住输入重放 | `D:\agent working\dafeiyu\notes\input-replay-migration-20261127.md` | `replayed=1`、0 失败 |
| 音频 | `D:\agent working\dafeiyu\notes\audio-first-step-20261127.md` | 迁移 572 包/569 帧/丢包 0 |
| 虚拟显示 | `D:\agent working\dafeiyu\notes\vdisplay-linux-app-window-20261127.md` | 120DPI、40,181 像素差异 |
| 图形面板 | `D:\agent working\dafeiyu\notes\manager-ui-20261127.md` | 三页签、Diagnostics 两行 OK |
| DEB 安装包 | `D:\agent working\dafeiyu\notes\deb-installer-accept-20261127.md` | 安装/许可/卸载全链路 PASS |
| 统一图标 | `D:\agent working\dafeiyu\notes\icons-all-platforms-20261127.md` | ICO/ICNS/mipmap 全端接线 |
| CI/SBOM/HUD/Manager | `D:\agent working\dafeiyu\notes\ci-sbom-ui-release-20261127.md` | 18+354 SBOM通过、9包单测全绿 |
| 非阻塞迁移与探测 | `D:\agent working\dafeiyu\notes\nonblocking-migration-probe-20261127.md` | 音频200/200零丢包、64B vs 1200B对照 |
| Linux RPM/systemd | `D:\agent working\dafeiyu\notes\linux-rpm-systemd-accept-20261127.md` | DEB/RPM + systemd active/healthz/purge |
| Windows 安装包构建 | `D:\agent working\dafeiyu\notes\windows-installer-upgrade-20261127.md` | 0.1.0/0.1.1 Setup.exe 构建与SHA-256 |

发布提交：`7f027c82aba0680e8e29593d8218156b1d4a0b76`；标签 `v0.1.0-source-available`。
