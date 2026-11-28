# macOS 原生后端与安装包真机开发验收记录（2026-11-28）

- **基准提交**：`7f1d67c1bd37e5323a4b3bcbac020097896c3708`（`main`，分支 `feat/macos-native`）
- **本机环境**：Apple Silicon (`aarch64-apple-darwin`)，macOS `27.2.1`（Build `26B131`），Xcode Command Line Tools SDK `27.2`，Apple Clang `21.0.0`，`rustc 1.100.1` / `cargo 1.100.1` / `python3 3.14.5`
- **许可证与依赖审计**：`PolyForm Noncommercial License 1.0.0`（`LicenseRef-PolyForm-Noncommercial-1.0.0`，非商用源码可见）；通过系统 Framework 直连 C FFI 与随 `cc v1.11.3` 编译的小型 Obj-C ARC 桥实现，**未新增任何第三方 crate**（`Cargo.lock` 第三方依赖数维持 354，SBOM 审计 PASS）。

---

## 1. 本次落地模块清单

| 模块 | 实现文件 | 关键能力 |
| --- | --- | --- |
| 硬件编解码 `dfy-codec` | `crates/dfy-codec/src/vt.rs`, `tests/macos_vt.rs` | Apple `VideoToolbox`（`VTCompressionSession` / `VTDecompressionSession`）低延迟 H.264 硬编优先/软编回退；BGRA8/RGBA8/NV12 输入转 `'420v'` NV12、Annex-B（IDR 附带 SPS/PPS）与 AVCC 兼容、`request_keyframe`、动态码率/帧率调整 |
| 屏幕采集 `dfy-capture` | `crates/dfy-capture/build.rs`, `src/macos_sck.m`, `src/macos.rs` | `CGGetActiveDisplayList` 物理显示器枚举（像素尺寸、主屏排序、Retina DPI）、`CGPreflightScreenCaptureAccess` 权限预检、`ScreenCaptureKit` (`SCStream`) BGRA8 采集；`dfy-session` 实现 `Capturer for MacCapturer` |
| 被控端输入 `dfy-input` | `crates/dfy-input/src/macos_keymap.rs`, `src/macos.rs` | 90+ 项 USB HID Usage (`0x07`) ↔ Apple `CGKeyCode` 双向映射（跨平台单测）；`AXIsProcessTrusted` 权限检查、`CGEvent` 键/钮/拖拽/滚轮/UTF-16 Unicode 注入、`release_all`/`Drop` 防卡键 |
| 控制端采集与链路 `dfy-cli` | `crates/dfy-cli/src/input_capture.rs`, `main.rs`, `real.rs`, `Cargo.toml` | `CGEventTap` + `CFRunLoop` 全局输入采集、`Ctrl+Alt+Shift+Q` 吞键停止、10ms 鼠标节流、`HeldInput` 迁移重放；`dfy view --ui` 在 macOS 主线程驱动 `winit` 事件循环；macOS `agent`/`view` 默认启用 H.264 与 `dfy-audio/device-backend` |
| 剪贴板 `dfy-clipboard` | `crates/dfy-clipboard/build.rs`, `src/macos_pb.m`, `src/macos.rs` | `NSPasteboard` `changeCount` 变化检测 + UTF-8/HTML/PNG（含 TIFF→PNG 兜底）/RTF/文件列表，接入 `ClipboardSync` 与 `LoopGuard` |
| 音频 `dfy-audio` | `crates/dfy-audio/src/lib.rs` | `CoreAudio`（`cpal`）设备层新增连续分块线性重采样 `MonoResampler` 与多声道上/下混（48 kHz 立体声 ↔ 16 kHz 单声道线格式），防边界爆音与队列积压 |
| 图形 UI `dfy-ui` / `dfy-manager` | `crates/dfy-ui/src/app.rs`, `crates/dfy-manager/src/main.rs` | 新增 `MainThreadViewer`、`ScaleFactorChanged` surface 脏标记与早期关闭保护；`dfy-manager` 支持 `.ttc` 字体集加载与 macOS 字体候选、Retina 缩放绘制与点击命中、`Cmd+Q`/`Cmd+W` 退出 |
| `.app` 与 PKG/DMG 打包 | `packaging/macos/{build-installer.sh,component.plist}`, `packaging/tests/smoke.py`, `.github/workflows/{ci.yml,release.yml}` | 当存在 `dfy-manager` 时组装 `/Applications/Dafeiyu Remote.app`（`Info.plist` 含 `NSScreenCaptureUsageDescription` / `NSMicrophoneUsageDescription` / `NSHighResolutionCapable`、`DFY.icns`、`dfy-manager`/`dfy`/`dfy-relay`）与 `/usr/local/bin/{dfy,dfy-relay}` 多包 `productbuild` PKG 及拖拽式 DMG；修复 `smoke.py`（macOS BSD tar `--force-local`）及 `packaging/windows/build-installer.sh`（macOS bash 3.2 空数组 `set -u`） |

---

## 2. 本机真实命令与验证证据

| 检查项 | 命令与环境 | 实测结果（退出码均 0，除非注明探测） |
| --- | --- | --- |
| VideoToolbox 实编实解 | `cargo test -p dfy-codec --test macos_vt -- --nocapture` | **5 passed / 0 failed**：`available_encoders` 检出 `Apple H.264 (HW)` 与 `Apple H.264 (SW)`；1280×720 编码 30 帧（27,400 B / 2 keyframes）全部解码回有效 NV12 像素；单帧低延迟无需 flush、AVCC 输入与动态码率通过 |
| NSPasteboard 剪贴板 | `cargo test -p dfy-clipboard -- --nocapture` | **29 passed / 0 failed / 1 ignored** + `clipboard_pull_roundtrip_over_quic` 1 passed + 1 doctest passed |
| CoreAudio + 重采样 | `cargo test -p dfy-audio --features device-backend -- --nocapture` | **12 passed / 0 failed**：`cpal host=CoreAudio`（2 inputs / 2 outputs），包含 48kHz 立体声↔16kHz 单声道重采样与设备播放单测 |
| ScreenCaptureKit 枚举与门控 | `cargo test -p dfy-capture -- --nocapture` | **6 passed / 0 failed / 1 ignored** (`live_sck_captures_bgra_frame`)；实机检出 `Display 1 (CG#1) 2940x1912 @(0,0) primary=true dpi=192`，越界索引与无权限错误信息精确命中 |
| CGEvent 输入映射与门控 | `cargo test -p dfy-input -- --nocapture` | **25 unit + 7 contract + 1 doctest passed / 0 failed**；90+ HID↔`CGKeyCode` 双向映射无冲突 |
| Manager HiDPI 与字体 | `cargo test -p dfy-manager` | **11 passed / 0 failed**（含 macOS `.ttc` 字体加载与 HiDPI 点击缩放单测） |
| CLI 单元与自测 | `cargo test -p dfy-cli` & `./target/debug/dfy selftest` | **46 passed / 0 failed**；`SELFTEST OK`（60 帧 / 120 DATAGRAM / RTT 0.152 ms） |
| TCC 权限与本机回环探测 | 无 TCC 授权的当前终端进程：`cargo test -p dfy-capture -- --ignored` 与 `dfy agent` + `dfy view` 回环 | 真实观察到本进程 `CGPreflightScreenCaptureAccess() == false` 与 `AXIsProcessTrusted() == false`；`dfy agent` 与 `dfy view` 完成 QUIC 握手（VideoToolbox H.264 + `NSPasteboard` 启用，RTT 1.973 ms），`ScreenCaptureKit` 与 `CGEvent` 因缺 TCC 权限明确报错并优雅停流/降级，未崩溃或卡死 |
| 真实 macOS `.app` PKG/DMG 构建 | `cargo build --release --locked -p dfy-cli -p dfy-relay -p dfy-manager` + `bash packaging/macos/build-installer.sh --binary-dir target/release --version 0.1.0 --arch arm64 --format all --output-dir .tmp/macos-pkg-verify` | **构建与验包均退出 0**：`dafeiyu-0.1.0-macos-arm64-unsigned.pkg`（13,051,027 B，SHA-256 `a52b3e991d357bbdcebd0ab25aee0be9f22270c5612a7aa12b30dbd2c89e7d6d`），`dafeiyu-0.1.0-macos-arm64-unsigned.dmg`（20 MiB，SHA-256 `d18b2c7838592e97ff162084d885a70dc099c671be61fff09b02aef4d8426ed1`）；`pkgutil --expand-full` 确认含 `component.pkg`（`/usr/local/{bin/dfy,bin/dfy-relay,share/doc}`）与 `app-component.pkg`（`Dafeiyu Remote.app`）；`hdiutil attach` 挂载 DMG 后实测 `plutil -lint Info.plist` OK、`Applications -> /Applications` 符号链接及内置 `dfy`/`dfy-relay`/`dfy-manager` 与 `dfy selftest`（RTT 0.117 ms / 120 DATAGRAM）均通过 |

---

## 3. 诚实保留的未验证边界

1. **TCC 交互式桌面实采与输入注入**：当前无头终端执行器未获得 macOS「屏幕录制」与「辅助功能」TCC 授权，故授权后的 `ScreenCaptureKit` 实时桌面抓帧、`CGEvent` 真实注入及 `CGEventTap` 全局热键仍需在用户交互桌面授权后现场复测（`cargo test -p dfy-capture -- --ignored` / `live_pasteboard_text_html_png_roundtrip`）。
2. **Mac ↔ Windows 跨物理机互控**：尚未在本机连线另一台物理 Windows（如已装 `v0.1.0` 的主机）跑真实跨机端到端视频/输入/剪贴板/文件/音频/路径迁移验收。
3. **GUI 真人桌面目视**：`dfy-manager` 与 `dfy view --ui` 的 Retina 实窗渲染、全屏/缩放及快捷键交互仍待可交互 macOS 桌面目视确认。
4. **macOS 虚拟显示（`dfy-vdisplay`）**：`CGVirtualDisplay` 为苹果未公开私有 API，无法满足公证与长期稳定性要求，继续诚实返回不支持错误，不伪造成功。
5. **macOS 音频捕获边界**：`dfy-audio` 使用 `cpal`/CoreAudio 采集默认**麦克风**输入（需 `NSMicrophoneUsageDescription`），不采集系统内录声音。
6. **代码签名与公证**：未配置 Apple Developer ID 证书与 Notary 凭据，PKG/DMG 保持 `-unsigned` 显式命名；实机首次从 `pkg`/`dmg` 安装后需配合 Apple ID 公证或本地允许策略运行。

---

## 4. `v0.2.2` 合入远端 UI 更新、CI/macOS 修复与本地真机校验（2026-11-28）

- **基线同步**：同步远端 `origin/main`（`82d38e2`，含 `eframe` 重构 `dfy-manager`、`dfy-ui` 工具条与嵌入字体、`assets/icon-source.png` 图标刷新、`Dafeiyu Remote.app` 显示名更新及精简版 10 个安装包 `release.yml`）。
- **修复项**：
  1. `crates/dfy-manager/src/main.rs`：`CREATE_NO_WINDOW` 常量补充 `#[cfg(windows)]`，修复 `ubuntu-latest` 与 `macos-latest` CI `clippy --all-targets -- -D warnings` 的 `dead_code` 报错。
  2. `crates/dfy-manager/src/main.rs`：补充现代 macOS 系统字体候选（`STHeiti Medium.ttc`、`Hiragino Sans GB.ttc`、`STHeiti Light.ttc`、`Songti.ttc`、`SFNS.ttf`、`SFNSMono.ttf`）与 `Monospace` 末尾中文回退，并新增无头 `egui::Context` 测试 `macos_install_fonts_renders_cjk_glyphs` 验证「大肥鱼远程」字形渲染非零宽且无替换符。
  3. `crates/dfy-manager/src/main.rs`：从 macOS `.app/Contents/MacOS` 或只读根目录 `/` 启动且未显式传参时，默认配置/设备书签路径落盘至 `~/Library/Application Support/Dafeiyu Remote/`，并在 macOS 过滤 Finder 启动注入的 `-psn_*` 参数。
  4. `packaging/tests/smoke.py`：对齐 `55e1c3d` 后精简版 10 个安装包（`release-assets/*` + `release-notes.md`）`release.yml` 契约，继续禁止 `|| true`。
  5. 版本统一升至 `0.2.2`（Workspace、`Cargo.lock` 与 `android/app/build.gradle.kts` `versionCode = 5` / `versionName = "0.2.2"`）。
- **本机 Apple Silicon (`arm64`, macOS `27.2.1`) 门禁与产物理实测**：
  - `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo clippy -p dfy-audio --no-default-features --all-targets -- -D warnings` 通过。
  - `cargo test --workspace` 全绿（含 `dfy-manager` 11 通过 / 0 失败、`dfy-ui` 78 单测 + 3 doctest 通过）、`cargo test -p dfy-codec --test macos_vt`（5 passed）、`cargo test -p dfy-audio --features device-backend`（12 passed）。
  - `cargo build --release --locked -p dfy-cli -p dfy-relay -p dfy-manager`：`dfy`/`dfy-relay`/`dfy-manager` 均输出 `0.2.2`；`./target/release/dfy selftest` 返回 `SELFTEST OK`（60 帧 / 120 DATAGRAM / RTT 0.202 ms）。
  - `python3 docs/requirements/split-and-verify.py --check` PASS（`sourceSha256 = 3db9a60032c0e801593be04f6094b376c97794521fa855d413acbade971cd4e3`）；`python3 packaging/generate_sbom.py --check` PASS（18 workspace crates / 598 第三方依赖）；`python3 packaging/tests/smoke.py` PASS。
  - 本地 `bash packaging/macos/build-installer.sh --binary-dir target/release --version 0.2.2 --arch arm64 --format all --output-dir .tmp/macos-pkg-verify` 通过：`pkgutil --expand-full` + `plutil -lint` 校验 `Dafeiyu Remote.app/Contents/Info.plist` OK（`CFBundleDisplayName` / `CFBundleName = 大肥鱼远程`，`CFBundleShortVersionString = 0.2.2`），内置 `dfy-manager` / `dfy` / `dfy-relay` 均为 `arm64` Mach-O；`hdiutil attach` 挂载 DMG 确认包含 `Dafeiyu Remote.app`、`Applications` 符号链接及 `.pkg`（本机候选 PKG SHA-256 `d71e293c8e4126ec38776501ea068cba6e9188b775d8ccfe6f241cc3c8d0a850`，DMG SHA-256 `8cffbcb1991ebdde2126cca1dcaaf6205facd6cedae520ce4a00ba73de0d089f`，验包后已清理临时目录）。
- **本轮边界**：仍无 Apple Developer ID 签名/公证；本机未在交互式桌面截图或做授权 TCC 后的 `dfy-manager` / `dfy view --ui` / `ScreenCaptureKit` 实屏互动目视，也未在本机重建 Android / Linux / Windows 安装包（由 GitHub Actions `CI` 与 `release` 负责）。
