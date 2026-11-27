# 交接文档（HANDOFF）

> 面向：接手 **大肥鱼远程 / Big Fat Fish Remote** 后续工程的下一个模型或工程师。
> 生成时间：2026-11-25。分支 `main`，起始 HEAD `dbaac796174a95ec593a9bcdd28dc5ba1bb10de6`，**尚未 commit、尚未 push**，全部改动在工作区（33 项，21 改 / 12 新增）。
> 本文只描述仓库与环境的客观现状；"通过"一律指**实际执行过的命令**，未执行的一律写"未测"。

---

## 0. 三十秒摘要

- 这是一个**已有相当代码量**的 Rust 远控项目（14 个 crate），不是从零开始的 prototype。
- 本轮（2026-11-25）做了三件事：**修复 CI 长期红灯**、**修补 10 项审计出的 P0 安全缺陷（含真实 QUIC 负测）**、**补齐 Linux X11 的输入注入与剪贴板后端并搭好真机 Linux 测试环境**。
- 本轮**没有**推进任何产品级新功能：虚拟屏仍是空壳、macOS/Android 为零、P2P↔中继的"可恢复会话迁移"未实现、多中继配置/一键部署/桌面 GUI 都还没做。
- 用户最终验收标准极高（见 §1），因此**现在绝对不能宣称"完成"**。

---

## 1. 用户目标（唯一权威来源，必须逐字遵守）

**原始需求原文（23342 字节，SHA-256 `3db9a60032c0e801593be04f6094b376c97794521fa855d413acbade971cd4e3`）已完整落盘：**

- `docs/requirements/000-original-user-prompt.md` ← **禁止修改**
- 已切成 28 个逐字模块：`docs/requirements/modules/001-positioning.md` … `028-decision-relay-modes.md`
- 入口 `docs/requirements/INDEX.md`；覆盖报告 `docs/requirements/COVERAGE.md`（825 行 / 583 非空行 / 242 非空块全覆盖，0 缺失 0 重复，拼接字节一致）
- 校验脚本 `docs/requirements/split-and-verify.py`（`--check` 只读复核）

**做某块功能前，先读对应模块文件，不要凭记忆。**

用户硬性约束摘要（细节以原文为准）：

1. 产品定位：开源、跨平台、**支持自建中继集群**的高性能远控系统；要"技术更好、更灵活、更易部署、更开放"。
2. 平台四端 × 双角色：Windows 10/11、Linux、macOS、Android 都要**既能控也能被控**（表中 Android 控制器/被控都是 Yes）。
3. 三大重点：①对标 UU 远程的功能体验（虚拟屏、跨平台快捷键映射、剪贴板、多显示器、远程输入、文件传输）；②网络自主可控（P2P 优先 + 质量差/失败自动切到**用户自己配置的**中继）；③中继极简化（一条命令装好，填地址+密钥即可）。
4. 虚拟屏是**最高优先级**，必须做成独立核心模块（无物理显示器可用、多虚拟屏、动态分辨率/刷新率、驱动崩溃自动恢复）。
5. P2P↔Relay 切换必须是**可恢复的会话迁移**，不能是断开重连。
6. 中继不得看到明文业务数据；口令/密钥不得明文存盘；要能撤销设备与终止会话。
7. 禁止：抽取专有源码、绕过鉴权、再分发专有组件。遵循各依赖许可证。
8. 工程纪律：不许只交计划、不许停在 MVP、不许把未测功能标为完成、不许伪造功能。

> 用户已授权：自行安装 WSL/WSA 调试、解包 `C:\Users\21903\Downloads\uuyc_4.48.2.apk`、`C:\Users\21903\Downloads\uuyc_4.48.4.pkg`，以及用 `wget -O install.sh https://uuyc.163.com/install.sh` 获取 Linux 版做合法研究（**解包产物只做研究，不能进仓库**，见 §7）。

---

## 2. 环境（已就绪，可直接用）

### 2.1 Windows 宿主

| 项 | 值 |
| --- | --- |
| 系统 | Windows 11 Pro 10.0.26300 x64 |
| 工作目录 | `D:\agent working`（仓库在 `D:\agent working\dafeiyu`） |
| Rust | stable-**x86_64-pc-windows-gnu**（msvc 也装了）；`x86_64-unknown-linux-gnu` target 已装 |
| cargo/rustc | 不在默认 PATH，必须用绝对路径 `C:\Users\21903\.cargo\bin\cargo.exe` |
| 链接器 | `C:\Users\21903\AppData\Local\Microsoft\WinGet\Packages\BrechtSanders.WinLibs.POSIX.UCRT_Microsoft.Winget.Source_8wekyb3d8bbwe\mingw64\bin` |
| gh | 已登录 `kanghelyu`（勿打印 token） |
| 其他 | git、node 24.20、pnpm、python 3.13、tar 有；**docker / java / adb 没有** |
| WSL | **Disabled**（`wsl --install` 返回 exit1，仅提示未安装；HypervisorPresent=True）——所以改用 QEMU |

**每条 Windows cargo 命令都要先设 PATH：**

```powershell
$env:PATH = 'C:\Users\21903\.cargo\bin;C:\Users\21903\AppData\Local\Microsoft\WinGet\Packages\BrechtSanders.WinLibs.POSIX.UCRT_Microsoft.Winget.Source_8wekyb3d8bbwe\mingw64\bin;' + $env:PATH
& 'C:\Users\21903\.cargo\bin\cargo.exe' <args>
```

### 2.2 Linux 真机测试环境（本轮已搭好，VM 正在运行）

- 位置：`D:\agent working\.tmp\dfy-linux-lab`（**已被 .gitignore 忽略**，不要提交）
- 形态：portable QEMU 11.1.0 + Debian 13 官方 cloud image（均通过发布方 SHA-512 校验），TCG 4 vCPU / 4GB / 20GB overlay，PID 15228（2026-11-25 14:14 启动）
- 内含：Rust/Cargo **1.100.0**、gcc **14.2.0**、**Xvfb :99（1280x720x24, -noreset -nolisten tcp）**、XTEST / XFIXES / RANDR
- 证据：`network_probe_result.json`、`guest_probe_result.json`（Windows↔Linux TCP/UDP 四路 echo 全通；真实指针读回 (111,222)；中文剪贴板往返通过；cloud-init done errors=[]）

```powershell
ssh -F 'D:\agent working\.tmp\dfy-linux-lab\ssh_config' dfy-linux-lab          # lab@127.0.0.1:22222
scp -F 'D:\agent working\.tmp\dfy-linux-lab\ssh_config' <本地> dfy-linux-lab:/home/lab/work/
python 'D:\agent working\.tmp\dfy-linux-lab\start_lab.py'   # 启动
python 'D:\agent working\.tmp\dfy-linux-lab\stop_lab.py'    # ACPI 关机
```

- 非交互 ssh 必须显式 `DISPLAY=:99`；guest 里 cargo 用 `/home/lab/.cargo/bin/cargo`，工作目录 `/home/lab/work`
- 端口：Windows→Linux 用 `127.0.0.1:24800` / `:24801`（**TCP 和 UDP 都转发**）；Linux→Windows 用 `10.0.2.2:<端口>`
- 密钥 `lab_ed25519` 只在 lab 目录内，**不要复制进仓库**

---

## 3. 仓库现状

### 3.1 结构

`Cargo.toml` workspace：edition 2021 / version 0.1.0 / **license LicenseRef-PolyForm-Noncommercial-1.0.0（非商用源码可见）** / rust-version 1.75。

| crate | 作用 | 本轮状态 |
| --- | --- | --- |
| `dfy-proto` | protobuf 消息、常量、权限位 | **改**：认证 v2 字段 |
| `dfy-transport` | QUIC 端点、TLS、握手、认证、CtrlChannel | **改 + 新增 `src/auth.rs`、`tests/auth_quic.rs`** |
| `dfy-relay` | Linux 中继（UDP/TCP、tag 槽位、管理面） | **改 + 新增 `tests/relay_security.rs`** |
| `dfy-capture` | Windows DXGI / Linux X11 GetImage+RandR 采集 | 未动 |
| `dfy-codec` | MF H.264 编解码 / MJPEG 兜底 | 未动 |
| `dfy-session` | agent/client 会话循环、FrameAssembler、CapturerLoop | 未动（迁移逻辑仍缺） |
| `dfy-input` | 键鼠事件、映射、注入 sink | **改 + 新增 `src/x11.rs`、`x11_common.rs`、`tests/x11_inject.rs`、`README.md`** |
| `dfy-clipboard` | 剪贴板同步状态机 + 平台后端 | **改 + 新增 `src/linux.rs`、`src/x11_state.rs`** |
| `dfy-files` | 文件传输 | 未动（单向） |
| `dfy-p2p` | STUN / 打洞 | 未动（**未接 CLI 自动路径选择**） |
| `dfy-path` | 路径质量策略状态机（43 测试） | 未动（**纯逻辑，未被 CLI 使用**） |
| `dfy-vdisplay` | 虚拟显示抽象 | **空壳**（Windows IDD 驱动只是骨架，无 WDK 签名构建） |
| `dfy-ui` | winit 查看器窗口 | 未动 |
| `dfy-cli` | `dfy` 命令（genkey/demo/selftest/agent/view） | **改 `real.rs`；新增 `src/credentials.rs`（尚未接线）** |

### 3.2 现有能力（源码存在 + 历史验证）

- Windows：DXGI 采集 → Media Foundation H.264 硬编 → QUIC DATAGRAM → 解码显示；SendInput 注入；剪贴板同步；单向文件传输；中继与 P2P 各有一条 CLI 路径。
- Linux：X11 抓屏（GetImage + RandR），Windows 之外的 MJPEG 兜底编码；**本轮新增** XTest/XKB 输入注入与 X11 剪贴板服务。
- 中继：单机 Linux 中继二进制，tag 会合、管理 HTTP、健康检查。
- **历史报告**（`design/014-real-pipeline-verified.md`、旧 README）：同机 6 秒链路跑通、输入/剪贴板/中继/文件同机实验。**本轮未复跑，不能当作当前回归通过，更不能外推到跨机。**

---

## 4. 本轮实际做完的事 + 证据

### 4.1 CI 红灯根因（已修）

`gh run view 45554721267 --log-failed` 显示 **macOS clippy** 失败：`crates/dfy-cli/src/real.rs` 中只在 `cfg(any(windows, target_os = "linux"))` 下使用的 `AgentConfig/CaptureLoop/serve_agent/start_capture_loop/ServerAuth/TlsIdentity/bind_agent/MftEncoderAdapter/AgentHandler/warn` 被无条件 import → `unused imports` → `-D warnings` → exit 101。
修复：在 `real.rs` 内按平台分组加 `#[cfg(...)]`，并 `cargo fmt -p dfy-cli`。
旁证：本机 `cargo check --workspace --all-targets` **exit 0**（job pwsh-583ce864c2824920）；早前 `cargo clippy --workspace --all-targets -- -D warnings` **exit 0**（job pwsh-7d133fad997442ae）。

### 4.2 安全审计（修补前快照）

`notes/security-audit-current.md`（121 行，只读源码分析，未执行攻击）。10 项发现：

| 编号 | 问题 | 本轮处置 |
| --- | --- | --- |
| A01 | 默认口令 `test` + 默认监听 `0.0.0.0` + 默认开启输入注入与剪贴板 | **未修**（CLI 改造进行中，见 §6 P0-2） |
| A02 | 无 pin 时接受任意证书；HMAC 只绑 nonce → 恶意中继拆成两条 TLS 实时转发即可读会话 | **已修**（认证 v2） |
| A03 | relay tag = `BLAKE3(password‖addr)[:16]`，可离线字典攻击 | **未修**（P0-2） |
| A04 | 剪贴板无逐连接批准；权限位图不执行；DataChunk 可无限累积 | **部分未修**（见 P0-3） |
| A05 | 口令明文打印；设备 seed / relay_key 明文落盘 | **未修**（P0-2 / P1） |
| A06 | relay nonce 缓存满载先淘汰后判重 → 重放 | **已修** |
| A07 | TCP 新连接覆盖热槽位后旧连接仍可 forward/touch | **已修** |
| A08 | SessionResume 只请求关键帧，无安全迁移语义 | **未修**（P1） |
| A09 | 管理监听只在 `config check` 时强制回环 | **已修**（运行时强制） |
| A10 | 串行握手无登录限流 | **未修**（P1，KDF 队列 2/8 只是内存保护） |

### 4.3 认证 v2（`crates/dfy-transport/src/auth.rs` 新增，`handshake.rs` 改）

- `EXPORTER-DFYP-1-AUTH-V2`：Argon2id **v0x13 / m=19456KiB / t=2 / p=1 / out=32**，salt 16 字节，**参数不协商**。
- 进程级 KDF 闸门：并发 2 / 排队 8；permit 移入 `spawn_blocking` 闭包，调用方取消也**不释放**配额（防绕过）。
- transcript = LP 字段序列：`DFYP-AUTH-V2 / auth_version / proto_major / proto_minor / client_name / device_pubkey / client_nonce / server_nonce / salt / "client" / "server" / exporter`。
- HMAC-SHA256 分域 `"server-proof"` / `"client-proof"`（防反射）；**客户端先验 server_proof，通过后才发 client_proof**。
- 协议字段（`dfy-proto/src/msg.rs`）：`Hello.client_nonce`(tag5)、`Challenge.salt`(tag2)/`server_proof`(tag3)/`auth_version`(tag4≈2)；`PROTO_MINOR=1`；`PERM_DEFAULT / permView`。
- 无降级：缺 `client_nonce` 或 `auth_version≠2` 一律拒绝。
- `ServerAuth` 的 `Debug` 打印 `[REDACTED]`；`ServerAuth::new` 默认只授 **VIEW**。
- 删除 `util` 中 nonce-only 的 `password_key/password_proof/verify_proof`（全仓已无调用方）。
- **测试**：16 unit + **11 个真实 loopback QUIC 安全测试** + 1 doctest 全过；clippy 过。覆盖：恶意双 TLS 中继转发、跨 exporter、角色反射、正确/错误口令、legacy `auth_version=0`、畸形字段、pin 错、KDF 取消不提前释放。

### 4.4 中继加固（`crates/dfy-relay/src/`）

- `auth.rs:75` `NonceCache::insert`：清严格过期 → 判重 → **满载直接 false**（不淘汰有效项）。
- `state.rs:41` `EndpointSource::Udp{addr,id}` / `Tcp{id}`；`forward`/`touch`/`unbind`（`:407`/`:453`/`:468`）校验 kind+id+UDP 地址；TCP 槽位被替换时旧 reader 停止、writer abort。
- `relay.rs:60` 真实启动入口强制管理监听 loopback（直接构造 Config 也绕不过）。
- **测试**：41 unit + 2 旧集成 + **6 新真实 loopback 安全测试** = **49 通过**；`cargo fmt -p dfy-relay -- --check`、`git diff --check` 通过。

### 4.5 Linux 输入（`crates/dfy-input/`）

- `src/x11.rs` / `x11_common.rs`：XTest 注入 + XKB 物理键名映射；5 鼠标键、相对移动、按根窗口实时缩放的绝对移动、双轴滚轮累积；`release_all` 保证断线不卡键、失败重试。
- `InputError` 新增 `UnsupportedSession / Unavailable / UnsupportedEvent`；**Wayland / XWayland 明确拒绝**；非空 `Text` 明确 unsupported。
- 测试：Windows 29 unit + 7 contract + 1 doctest 通过；`--target x86_64-unknown-linux-gnu --all-targets` 交叉 check 通过。**Linux runtime 未执行。**
- 已知限制：根坐标轴 >32768 时无法绝对移动；物理 XKB 重配需重建 sink；真实桌面/多屏/IME 未验。

### 4.6 Linux 剪贴板（`crates/dfy-clipboard/`）

- `src/linux.rs` / `x11_state.rs`：XFixes 所有权事件驱动的 X11 选择区服务，**只枚举 TARGETS 元数据**、按需拉取载荷、INCR 有界。
- 测试：Windows 20 unit + 1 QUIC 集成 + 1 doctest 通过；Linux 交叉 check 通过。**Linux runtime 未执行**（唯一 native 测试 `isolated_x11_roundtrip_and_owner_lifetime` 是 `--ignored`）。
- 已确认的遗留缺陷（**未修**）：`sync.rs` 的 `on_remote_chunk` 只在末尾检查 `MAX_SYNC_BYTES`，`last=false` 可无限累积；未知 hash 默认当 `CF_UNICODETEXT`；最终 BLAKE3 未校验。

---

## 5. 未提交改动清单（33 项）

```
 M Cargo.lock                                  (自动新增 argon2 0.5.3 / blake2 0.10.6 / password-hash 0.5.0 等，勿回滚)
 M crates/dfy-cli/src/real.rs                  (CI 修复：平台 cfg 化 import)
 M crates/dfy-proto/src/{consts.rs,msg.rs}     (认证 v2 字段、PROTO_MINOR=1、PERM_DEFAULT)
 M crates/dfy-transport/{Cargo.toml,src/lib.rs,src/handshake.rs,src/tls.rs,src/util.rs}
 M crates/dfy-relay/src/{auth.rs,config.rs,relay.rs,state.rs,tcp.rs}
 M crates/dfy-input/{Cargo.toml,src/lib.rs,src/event.rs,src/unsupported.rs}
 M crates/dfy-clipboard/{Cargo.toml,src/lib.rs,src/sync.rs}
?? crates/dfy-cli/src/credentials.rs           ← 我写的，尚未接进 main.rs
?? crates/dfy-transport/src/auth.rs
?? crates/dfy-transport/tests/                 (auth_quic.rs)
?? crates/dfy-relay/tests/relay_security.rs
?? crates/dfy-input/{README.md,src/x11.rs,src/x11_common.rs,src/x11_common/,tests/x11_inject.rs}
?? crates/dfy-clipboard/{src/linux.rs,src/x11_state.rs}
?? docs/                                       (整棵需求/状态/研究文档树，应提交)
```

> `crates/dfy-input/src/x11_common/` 是目录，内部还有文件，提交前用 `git add -A crates/dfy-input` 确认没有漏。

**上一次 `cargo test --workspace` 被中途 kill（未得到结论），需要重跑。**

---

## 6. 下一步优先级（已排序）

### P0-1 收口本轮改动（半天内可完成）

1. `cargo test --workspace --offline`（Windows）+ `cargo clippy --workspace --all-targets -- -D warnings` + `cargo fmt --all -- --check`，全部绿灯。
2. 在 Linux guest 上跑真实测试（这是本轮唯一"代码写了但没证明能用"的地方）：
   ```bash
   # 先把工作区同步进 guest（rsync/scp 到 /home/lab/work/dafeiyu），再：
   cd /home/lab/work/dafeiyu
   DISPLAY=:99 XDG_SESSION_TYPE=x11 DFY_INPUT_X11_TEST=1 /home/lab/.cargo/bin/cargo test -p dfy-input --test x11_inject -- --ignored --test-threads=1
   DFY_CLIPBOARD_TEST_DISPLAY=:99 /home/lab/.cargo/bin/cargo test -p dfy-clipboard -- --include-ignored --nocapture
   ```
   失败就把输出发回给对应作者范围（x11.rs / linux.rs）修。
3. 分 3 个 commit：`fix(ci)` / `fix(security)` / `feat(linux)` + `docs`。然后再 push 触发 CI，确认三平台绿灯。

### P0-2 CLI 安全收口（**已开工，见 `crates/dfy-cli/src/credentials.rs`**）

已有：`PasswordArgs`（`--password` / `--password-stdin`，交互终端用 rpassword 隐藏输入）、`SessionPassword(Zeroizing<String>)`（16–1024 字节、Debug 打 `[REDACTED]`、drop 清零）、`read_line` 测试覆盖 CRLF/超长。

还差（照这个清单改 `main.rs` + `real.rs`）：

1. `mod credentials;`，`dfy-cli` 加 `rpassword` / `zeroize` 依赖。
2. `Agent` / `View` / `DemoAgent` / `DemoClient`：删掉 `default_value = "test"`，改用 `PasswordArgs`；`AgentOptions.password` / `ViewOptions.password` 改成 `SessionPassword`（或传 `&str` 并从 Debug impl 里去敏）。
3. **删掉所有打印口令的代码**：`real.rs:215-218` 的 `[agent] 口令 = {}`、`main.rs:413` 的 `[demo-agent] 口令 = …`。
4. 默认值翻转：agent 默认监听 `127.0.0.1`（而非 `0.0.0.0`）；输入注入与剪贴板**默认关闭**，需显式 `--allow-input` / `--allow-clipboard`。
5. 权限位对齐：`ServerAuth.permission_bitmap` 必须等于实际启用的能力（关掉输入就别给 `INPUT` 位）。现在是 `ServerAuth::new` 固定给默认值。
6. A03：relay tag 不再由口令派生 —— 改为 `--relay-tag` 显式给出，或首次随机生成后打印/存配置。
7. `run_view` 传真实设备公钥，替换 `client_handshake(..., vec![0u8;32])`。
8. `build_relay` 支持主机名（现在是裸 `SocketAddr::parse`），与文档承诺一致。
9. A04/D4.6：剪贴板首帧不自动上传本地已有内容（需批准或"仅文本/尺寸上限"策略）；`on_remote_chunk` 每块前置累加校验上限；校验最终 BLAKE3；未知格式哈希直接丢弃。

### P0-3 虚拟屏（用户点名最高优先级，当前基本为零）

- `crates/dfy-vdisplay` 只有抽象；Windows 侧需要 **IDD 间接显示驱动**（`design/` 里是骨架，**没有 WDK 签名构建**，本机也没验证过能编）。风险点：测试签名证书 + 驱动安装会改系统显示配置，**必须在测试机/VM 里做，不能动用户主线显示器**。
- Linux：`VKMS`（内核模块，需 root/modprobe）或 X11 `Xvfb`/`Xephyr`/`xrandr --setmonitor` 组合；Wayland 侧需看 mutter/gnome-remote-desktop 是否给虚拟输出。
- macOS：`CGVirtualDisplay`（未公开 API，风险高）或 ScreenCaptureKit + 需要用户授权；受 SIP/权限限制。
- 统一接口建议：`VDisplayManager { list, create(spec), resize(id, w, h, hz), destroy(id), health/recreate }`，上层只依赖这个 trait。

### P1 其余大块（按用户优先级）

1. **P2P↔Relay 可恢复迁移**：`dfy-path` 的 43 测试状态机目前**完全没接进 CLI**；`SessionResume` 只请求关键帧，没有会话 token / 设备身份 / 权限代际绑定 → 必须重做协议（服务端保持会话状态跨连接、`session_id + epoch` 校验、输入/文件/剪贴板状态续传）。agent 现在是**串行 accept 循环**，两条路径并发必须先改架构。
2. **多中继配置 + 智能选路 + 故障切换**：客户端配置模型（显示名/地址/端口/密钥/TLS/区域/优先级/健康）+ 延迟探测 + 策略（手动/最低延迟/优先/自动故障转移）+ 导入导出 + UI 显示当前中继。
3. **中继极简部署**：一键安装脚本、交互式向导、TLS（ACME）、防火墙/端口诊断、`systemd` 单元、`docker compose`、升级/卸载、健康检查。**注意**：`deploy/README.md` 里的示例配置键（`admin/tag_ttl_s/max_tags/per_tag_bps`）与现行 `config.rs`（`admin_addr/tag_ttl_hours/max_sessions/per_tag_mbps`）**不一致**，要一起改。
4. **桌面 GUI**：现在是 winit 极简查看器 + CLI，距离"商业级观感"很远（设备列表、最近连接、收藏、搜索、连接统计面板、中继管理页）。
5. **macOS / Android 双角色**：macOS 采集/注入/权限全缺；Android 需要 Kotlin 工程、MediaProjection（被控）、无障碍/输入注入、外部键鼠、后台限制说明。APK/PKG 解包材料可用于合法研究（见 §7）。
6. **A08/A10 与密钥存储**：设备身份持久化 + 配对 + 撤销 + OS 凭据库（Windows DPAPI / Linux Secret Service / macOS Keychain）；中继重启 epoch 防重放；外层 TCP TLS；登录限流。

---

## 7. 仓库卫生与红线

- `.gitignore` 覆盖：`/target`、`.tmp/`、`*.exe`、`.env`、`/research/`、`/notes/`、`device.key`、`device.pub`、`*.log`、`deploy/relay.toml`。
- `research/`、`notes/` 里有大量**解包出来的 UU 专有二进制/配置**，已被忽略：**绝不能提交、不能再分发**。本轮新写的 `notes/security-audit-current.md` 也在忽略区，若要长期保留请把**去敏结论**移到 `docs/`。
- 不要提交：任何密钥、`lab_ed25519`、`device.key`、relay 配置里的真实密钥、CI token。
- 许可证：workspace 是 **PolyForm Noncommercial License 1.0.0（非商用源码可见，非 OSI 开源）**；新依赖许可证已由 cargo 记录在 `Cargo.lock`，发布前仍要做一次全量 license 审计（非商用条款与 App Store 商业分发存在冲突，Android 上架策略要提前想）。
- 工具环境约束：approval 已禁用，**不要请求 sandbox escalation**；pwsh 调用不要带只给 justification 的字段。
- 多个 agent 并行时：各自用独立 `CARGO_TARGET_DIR`，只 `cargo fmt -p <自己的 crate>`（**禁止 `fmt --all`**），不要同时动 `Cargo.lock`。

---

## 8. 已验证 / 未验证 一览（给验收用）

| 项 | 状态 |
| --- | --- |
| Windows 工作区编译 | ✅ 本轮 exit 0 |
| Windows workspace 全量测试 | ⚠️ **本轮未跑完**（需重跑） |
| transport / relay / input / clipboard 单 crate 测试 | ✅ 各自代理实跑（数量见 §4） |
| Linux 交叉编译（input、clipboard） | ✅ |
| Linux X11 输入真实注入 | ❌ **未执行**（环境已就绪，命令见 P0-1） |
| Linux X11 剪贴板真实往返 | ❌ **未执行**（同上） |
| Windows↔Linux 端到端远控（跨机/P2P/Relay） | ❌ 未测 |
| 虚拟屏（三平台） | ❌ 未实现 |
| 会话迁移（P2P↔Relay） | ❌ 未实现 |
| macOS / Android | ❌ 未实现 |
| 一键部署 / 安装升级 | ❌ 未实现 |
| 发布（Release、安装包、CI 绿灯） | ❌ 未做 |

**在 P0-1 的 Linux 真机测试和一次完整的跨机链路验收之前，任何"功能完成"的表述都属于虚假声明。**

