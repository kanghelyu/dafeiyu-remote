# 交接文档（HANDOFF，2026-11-26 最终复核包）

> 面向接手 **大肥鱼远程 / Big Fat Fish Remote** 的下一个模型或工程师。
> 仓库：`D:\agent working\dafeiyu`；分支 `main`；HEAD `dbaac79`；**未 commit、未 push**（工作区 85 项状态）。
> 上一版完整基线见 [`HANDOFF-2026-11-25.md`](HANDOFF-2026-11-25.md)。本文件描述 2026-11-26 Android 增量、独立复核修复与最终真实验收状态；与旧文件冲突时，以本文件为准。
> “通过”只指下面实际执行过的命令；未执行的一律标为未测。**整体产品完成度仍然为假，不能对外宣称完成。**

## 0.1 2026-11-26 续作补记

- 新增 `docs/research/UU-OBSERVED-FEATURES.md`：只按 O/I/U 整理已解包 UU 制品的功能面，不含专有二进制、私有协议常量、密钥、端点或账户数据。
- Windows 控制端输入：5ms 光标/按键状态轮询兜底（钩子继续负责滚轮），与钩子共用坐标/按下集合去重；`cargo test -p dfy-input -p dfy-cli --lib --bins` 与 strict clippy 通过。真实诊断确认本机 `SendInput` 合成键钮不进入钩子/Raw Input，故 Windows 合成键钮采集仍未验证，需要可交互物理桌面复核。
- Linux 控制端输入：XInput2 raw 优先，失败时降级 X11 根窗口核心事件；隔离 Xvfb 真实测试通过指针、左键、`A` 键、上下滚轮、`Ctrl+Alt+Shift+Q` 停止。Linux `dfy-input` 31 单测、CLI 2 单测＋1 ignored 实测通过；guest 未安装 clippy。
- 跨 OS 证据：Windows↔Linux 同机 QEMU 双向视频/指针/键盘/Unicode 剪贴板/文件哈希，详见 `notes/crossplatform-acceptance-20261126.md`、`docs/STATUS.md`、`docs/FEATURE-MATRIX.md`。仍不是两台物理设备、公网 NAT/Relay、弱网长稳、120fps/12Mbps、Wayland/macOS/Android 真机或生产发布验收。
- 用户中继池：新增 `dfy-relay-profiles` crate 与 `dfy relay-profile` CLI，`agent/view --relay-profile` 可从档案读取服务器/密钥并显示当前服务器；本机真实 relay 的 add/list/test/use/select/export/import 通过，密钥脱敏与 0600 已接线，见 `notes/relay-profiles-acceptance-20261126.md`。
- 可恢复路径迁移：`agent/view --auto-path` 已接入 `dfy-path`、`client_resume_handshake` 与共享采集代际；本机真实双向强制迁移（直连→中继、中继→直连）和真实拥塞代理无强制开关自动迁移均通过（epoch=1、恢复确认、旧会话 migrated、帧连续），见 `notes/auto-path-acceptance-20261126.md`。在途业务状态恢复仍未验收。
- Linux 虚拟显示后台监视：Agent 新增 5s 健康检查任务；无 Viewer 连接时杀掉受管 Xvfb，8s 内由后台任务重建（先用 `ps` 确认，未触发 `vdisplay list` 自恢复）。
- 音频链路：`--audio-out/--audio-in` 已接入 agent/view，100ms 抖动缓冲按 seq 重排/计丢包/自适应，`--volume/--mute` 软件增益，迁移时重置序号并丢弃旧纪元包。跨 OS 直连 448 包/445 帧；强制迁移新纪元 572 包/569 帧/丢包 0。先前“814 收/571 播”已定位为旧纪元积压包误入缓冲后被清空，已修复。本机无输入设备，用 `--audio-test-tone` 诊断源替代麦克风，见 `notes/audio-first-step-20261127.md`。
- Linux DEB 产品化：packaging/linux 新增 prerm/postrm 与 `verify-deb.sh`；guest 真实跑通构建→元数据/许可校验→dpkg -i→两ELF帮助→dpkg -r 清理，见 `notes/deb-installer-accept-20261127.md`。
- 图形化管理面板第一步：新增 `crates/dfy-manager`，winit+softbuffer+ab_glyph 显示中继档案健康与虚拟显示；字体候选链、C/D 预设虚拟屏创建/销毁、只读 Diagnostics 一致性页与 `--tab` 直达；Windows 真实起窗截图与 7 单测通过，见 `notes/manager-ui-20261127.md`。击键路径因共享桌面焦点不稳定未验收。
- 中继健康真实丢包/带宽：`relay-profile test` 用随机 tag 的一对 Client/Agent socket 做配对 RTT + 8×1200B 数据面探测，写回并打印 loss/bps；本机实测 0ppm/3.84Mbps（小突发观测值，非容量测量），见 `notes/relay-profiles-acceptance-20261126.md`。
- 迁移瞬间按住输入状态恢复：采集线程共享 HeldInput 快照，迁移后在新通道重放按住键/按钮；注入端对重复 Down 幂等。Linux 主控→Windows 被控强制迁移实测 `replayed=1`、0 失败，见 `notes/input-replay-migration-20261127.md`。鼠标绝对坐标在途重放仍未做。
- Linux 虚拟显示：Xvfb 启动新增 `-dpi`，xdpyinfo 读回 120DPI；在 144DPI 虚拟屏启动 xmessage 后，View 两帧相差 40,181 像素，证明真实应用窗口上屏，见 `notes/vdisplay-linux-app-window-20261127.md`。刷新率仍为元数据，Wayland/复杂 GUI 未验。
- 候选路径主动探测：`RequestProbe` 已接轻量 QUIC 探测并回填 RTT/丢包，实测弱网迁移后多次探测并正确请求回切；探测口径局限见 `notes/active-path-probe-20261127.md`。
- 跨 OS 迁移后双向剪贴板：Windows View ↔ Linux Agent 强制直连→中继，epoch=1、恢复确认、帧#23/23；迁移后两个方向各 35B Unicode 文本成功同步，见 `notes/crossos-migration-clipboard-20261127.md`。此前“迁移后剪贴板未稳定复现”是测试拓扑错误（同机共用剪贴板却检查 Linux），另加迁移握手超时 5s→15s 才让 QEMU NAT 路径可用。
- 未 commit、未 push；所有测试口令只经 stdin，未写入仓库或证据报告。

---

## 0. 三十秒摘要

- 用户要求：全端（Windows / Linux / macOS / Android）双角色、独立安装包、真实可用虚拟屏、自定义分辨率/码率/最高 120fps、弱网自适应推荐、UU 远程等价功能。
- 本轮完成 Android 被控端/查看端生命周期修复、自适应确认/回滚硬化、预测性 Back、共享采集 stop/activity 原子化，以及会话迁移 COMMIT 丢失确认后的断线幂等恢复。
- 最终 APK 已构建、安装并在 API36 模拟器上重新取得全屏 MediaProjection 授权；Host 与 Viewer 当前仍在运行。
- 最终包已取得 **非黑解码像素**、**画质 Apply/Reset**、**硬件 Back 不退出 Viewer** 证据。
- 独立只读复核历经多轮，最后一轮结论：**无剩余具体缺陷**；仅保留下述测试缺口。
- **持续 120fps/12Mbps、弱网、真机/跨机、进程死亡授权恢复、预测性边缘手势、全平台功能仍未完成。**

---

## 1. 当前运行状态（交接时）

| 项 | 值 |
| --- | --- |
| 模拟器 | `emulator-5554`，`sdk_gphone64_x86_64`，API36 |
| 应用 | `remote.dafeiyu`，versionCode 1，targetSdk 36 |
| 最终 APK SHA-256 | `f5aad85a24fff0a12a7ca2b24e14441b5e435da66c3eb0c912d5bba02fa815de` |
| 构建产物 | `android/app/build/outputs/apk/debug/app-debug.apk`（19,004,569 bytes） |
| 设备 APK 副本 | `.tmp/installed-apk-security-final-20261126.apk`，SHA-256 与上值一致 |
| Host 进程 | PID `20871` |
| 前台服务 | `RemoteService`：`isForeground=true foregroundId=71 types=0x00000020`，`startRequested=true` |
| Host 状态 | `state=capturing epoch=1 detail=1080×1920 H.264`，Viewer 已连接 |
| Viewer 状态 | `State: connected`；自由窗口 task `168`，bounds `Rect(320,60)-(1920,1020)` |
| TLS 指纹 | `cbe135b3e461548b82b1708499aad60c67b6f9273bc55d0ab3493821628c9d0a` |
| 身份密文 | `files/host_identity.pkcs8.enc`，`0600`，SHA-256 `e855b2280ddd4f495fe294855ce8e960ecb119c66cc3723d92ee4a0f6b1dc061` |
| Host 本次启动上限 | 最大 1920×1920，120fps，12000kbps |
| 当前确认画质 | Reset 后 `1080×1920 / 120fps / 12000kbps` |

最终授权输出：

```text
field set: 请输入本次会话口令=<redacted>
field set: 最大宽度（偶数）=1920
field set: 最大高度（偶数）=1920
field set: 帧率（1–120）=120
field set: 码率 kbps（100–100000）=12000
projection consent: ['Share one app', 'Next']
projection consent: ['Share one app', 'Share entire screen']
projection consent: ['Share entire screen', 'Share screen']
projection_host_idle fingerprint=cbe135b3e461548b82b1708499aad60c67b6f9273bc55d0ab3493821628c9d0a
```

证据：`.tmp/dfy-android-quality-live-20261126/host-start-security-final-20261126.log`

---

## 2. 本轮最终改动

### 2.1 Android 解码器与 Viewer

- `DfyH264Decoder.kt`
  - `codecGeneration` / `retiringCodec` 覆盖正常退役、超时、异常 detach、Surface/release 与 join 超时。
  - 旧 codec 停止/释放完成前不创建替代 codec；worker `finally` 只在 `codec == null` 时清栅栏。
  - 最终包实机观察到 `decoder_retired` 后重新 `decoder_selected` 并输出非黑帧。
- `ViewerActivity.kt`
  - 改为 `ComponentActivity` + 按权限启用的 `OnBackPressedCallback`，移除仅拦截 key event 的旧 Back 路径。
  - `ACTION_UP/CANCEL` 始终清理手势和远端按键状态。
  - 最终包硬件 Back 实测后仍 `State: connected`。

### 2.2 自适应画质确认/回滚

- `adaptation.rs`
  - 统一 `apply → 确认 → 越界回滚 → 再确认`；普通越界回滚成功可继续，回滚失败/恢复值仍越界返回 `FatalSettingsConfirmation`。
  - Agent 在致命确认错误下以 `quality_confirmation_failed` 结束会话并停止共享采集。
  - 已知 source 会约束平台确认值；Reset 不会覆盖已知 source。
  - source 缺失时只从启动原生确认或 Reset-to-native 补全，不再被用户上限/自适应降级污染。
- `capture_loop.rs`
  - `CaptureActivityTransition` 增加 `stopped`；stop、acquire、pending acquire、activate/deactivate 在同一 transition 锁下完成。
  - 停止后新会话不会激活采集；迟注册 observer 不会误报 active；pending 激活可回滚。

### 2.3 会话迁移/丢失确认恢复

- `migration.rs`
  - COMMIT 激活 hook 在 grant/coordinator/lease 变更之前执行；失败不提交、不发 `SessionResumeOk`。
  - `MigrationAckFailed` 区分“已提交但确认发送失败”，Agent 保留提交与激活状态等待幂等重试。
  - 无活动连接的新 lease 可在 grant TTL 内 reclaim 已提交会话，保持 epoch/token/generation/帧进度。
- `resume.rs`
  - committed 请求保存完整字段；重放先验证新连接签名，再比较除签名外的请求字段。
  - 重放必须绑定原设备公钥；`commit()` 入口清理过期 committed 条目。
  - 已测试同设备新签名重放、字段篡改拒绝、不同设备拒绝、过期拒绝、断线 reclaim 后续帧连续。

### 2.4 仍未覆盖的确定性测试

- 解码器异常 detach、`stop/release` 超过 `STOP_JOIN_MS`、Surface 替换竞态没有纯确定性故障注入测试；已有实机正常退役/重建与超时 fallback 证据。
- 预测性 Back 边缘手势未单独自动化；硬件 Back 已实测。
- 迁移不同设备/过期拒绝目前是 transport 单测，未做完整网络 reclaim 攻击测试。
- 没有并发双 reclaim 压测；共享状态 mutex 已静态复核。
- 没有 agent 级“ack fail → 断线 → 第三连接 reclaim”全链路自动化；SessionLease/transport e2e 已覆盖关键状态转换。

---

## 3. 本轮实际执行的测试

### 3.1 Rust 最终回归

```bash
cd 'D:/agent working/dafeiyu'
cargo fmt -p dfy-transport -p dfy-session -p dfy-android -- --check
cargo test -p dfy-transport -p dfy-session -p dfy-android
cargo clippy -p dfy-transport -p dfy-session --all-targets -- -D warnings
```

结果：

- `dfy-android`：17 passed / 0 failed
- `dfy-session` unit：66 passed / 0 failed
- `dfy-session` e2e：8 passed / 0 failed
- `dfy-session` doctest：1 passed
- `dfy-transport` unit：20 passed / 0 failed
- `dfy-transport` auth QUIC：11 passed / 0 failed
- `dfy-transport` resume QUIC：1 passed / 0 failed
- `dfy-transport` doctest：1 passed
- strict clippy：通过

日志：`.tmp/dfy-android-quality-live-20261126/rust-tests-security-final-20261126.log`

### 3.2 Android 最终构建

```bash
JAVA_HOME='D:/agent working/jdk-17'
ANDROID_HOME='D:/agent working/AndroidSdk'
GRADLE_USER_HOME='D:/agent working/dafeiyu/.tmp/gradle-user-review'
TMP='D:/agent working/dafeiyu/.tmp/cc-temp-review'
CARGO_TARGET_DIR='D:/agent working/dafeiyu/.tmp/cargo-target-android-review-fixes-20261126'
# 使用已解压 Gradle，避免用户 .gradle wrapper/native lock 权限问题
'C:/Users/21903/.gradle/wrapper/dists/gradle-8.11.1-bin/bpt9gzteqjrbo1mjrsomdt32c/gradle-8.11.1/bin/gradle.bat' \
  -p android :app:testDebugUnitTest :app:assembleDebug
```

结果：`BUILD SUCCESSFUL`；8 个测试类 / 34 tests，0 failures / 0 errors / 0 skipped。仅有既有 deprecation 与 `dfy-android` 旧 `bind`/`bind_with_identity` dead_code 警告。

### 3.3 最终 Viewer / 画质 / Back

- 自由窗口 + 左侧 Settings 可见时，最终包 `DfyViewerPixels`：
  - `frame=1/15/30/60`
  - `size=763x586`
  - `max=254`
  - `non_black_ratio=176/1776`
- 最终包 Apply：`1280×720,120fps,12000kbps` → 平台确认 `404×718,120fps,12000kbps`。
- 最终包 Reset：回到 `1080×1920,120fps,12000kbps`。
- 最终包硬件 Back：Viewer 保持 `State: connected`。

证据：

- `.tmp/dfy-android-quality-live-20261126/viewer-security-final-nonblack-20261126.log`
- `.tmp/dfy-android-quality-live-20261126/viewer-security-final-logcat-20261126.log`
- `.tmp/dfy-android-quality-live-20261126/quality-security-final/report.json`
- `.tmp/dfy-android-quality-live-20261126/quality-controls-security-final-20261126.log`
- `.tmp/dfy-android-quality-live-20261126/viewer-security-final-after-hardware-back-20261126.xml`
- 汇总：`.tmp/dfy-android-quality-live-20261126/continuation-evidence-20261126.md`

### 3.4 120fps/12Mbps 有界观察（不是通过）

- 配置为 `1080×1920,120fps,12000kbps` 的静态 Settings 画面，78.412 秒采样仅 2,432 帧：`31.016 fps`、`8,507.142 bytes/s`。
- 后续修复包另一次 78.324 秒静态采样为 `28.535 fps`、`12,631.841 bytes/s`。
-  motion 刺激尝试因触摸全屏 Settings 导致自由窗口 Viewer 停止而无效，已标 `valid:false`。
- **这些数据不能证明持续 120fps/12Mbps，也不能反推编码器上限；持续/运动画面/弱网仍未验收。**

证据：`.tmp/dfy-android-quality-live-20261126/sustained-120-*.json`

---

## 4. 当前明确的未完成/未验证项

### Android 立即要做

1. **持续 120fps/12Mbps 与弱网**：未测；当前只有配置接受、静态画面有界采样。
2. **真机/跨机/公网**：未测；当前均为 API36 模拟器同机 loopback。
3. **预测性 Back 边缘手势**：未单独自动化；硬件 Back 已通过。
4. **进程重建**：MainActivity fail-closed 逻辑存在，但“授权期间进程被系统杀死”的自动化回归未做。
5. **解码器故障注入**：异常 detach、join 超时、Surface 替换仍缺确定性测试。
6. **并发迁移攻击/恢复**：双 reclaim 压测、不同设备/完整网络过期攻击未做。
7. **多虚拟屏/独立 headless 屏**：Android 单个 MediaProjection 仍不等于 UU 式多虚拟屏。
8. Android 远程输入仍只有点击/滚动/有限全局 Back/Home；无全局键盘、剪贴板、文件传输、拖拽。

### 全平台未完成

- macOS 原生 Host/Controller/虚拟屏：仍无原生实现。
- Linux Xvfb 虚拟屏：controller→Agent 在该虚拟屏上的输入注入仍未完成/未验收。
- Windows：WGC 主路径、DXGI 初始化 fallback 有代码；运行时 WGC→DXGI 故障切换未完成。
- 跨平台安装包、升级、发布签名、完整验收矩阵未全部完成。
- 真实 P2P/中继跨机/可恢复迁移的生产级验收未完成。
- **整体：不要对外说“全端完成”。**

---

## 5. 环境与操作注意事项

- Android 工具：
  - `adb`: `D:/agent working/AndroidSdk/platform-tools/adb.exe`
  - `JAVA_HOME=D:/agent working/jdk-17`
  - `ANDROID_HOME=D:/agent working/AndroidSdk`
- Cargo/Gradle 并行使用独立 target；不要运行 `cargo fmt --all`，不要同时改 `Cargo.lock`。
- Windows 临时目录使用 `.tmp/cc-temp-review`；直接使用已解压 Gradle + 独立 `GRADLE_USER_HOME`，避免用户 `.gradle` lock/native 权限问题。
- 授权脚本：`.tmp/android_grant_projection.py`。
  - 必须明确选择 `Share entire screen`；进入 `Choose app to share` 要 fail closed。
  - `uiautomator dump` 可能改变旋转并暂时禁用无障碍服务，点击授权前调用 `restore_accessibility()`。
- **同设备全屏 Viewer 自捕获会产生黑帧**：系统会显示 `App content hidden from screen share for security`，且 Viewer 覆盖采集面。
  - 验收用横屏自由窗口 + 左侧 Settings：
    - `settings put system user_rotation 1`
    - `am start -n remote.dafeiyu/.ViewerActivity --windowingMode 6 --activity-multiple-task --ez dfy_capture_probe true`
    - `am task resize <task-id> 320 60 1920 1020`
  - 脚本：`.tmp/android_viewer_landscape_once.py`、`.tmp/android_quality_controls_landscape.py`。
- 当前日期 2026-11-26。真正仓库是 `D:\agent working\dafeiyu`；Qoder 工作区不是 Git 仓库。
- 测试口令只用于临时验收，不写入 app/仓库。

---

## 6. 安全/合规约束（仍然有效）

- `docs/requirements/000-original-user-prompt.md` **禁止修改**；最终 SHA-256 仍为 `3db9a60032c0e801593be04f6094b376c97794521fa855d413acbade971cd4e3`。
- 解包产物只做研究，不能进仓库；`D:\agent working\dafeiyu-reference\uuyc-*` 不得再分发。
- 不得提交/再分发专有组件；不得提交任何密钥、`lab_ed25519`、`device.key`、真实 relay 密钥、CI token。
- 未获得明确授权前不要 commit/push。
- 不要承诺操作系统不支持的“无限控制”；Android 当前受 MediaProjection/无障碍能力限制。

---

## 7. 下一位接手者的最短路径

1. 先读本文件与 `docs/requirements/INDEX.md` 对应模块，不要凭记忆扩展需求。
2. 当前 Host/Viewer 仍在运行；若要重启 Host，使用 `.tmp/android_grant_projection.py`，通过环境变量传入临时口令/指纹/上限，不写入仓库。
3. 复测 Viewer 必须使用 §5 的横屏自由窗口方案，不能把全屏同机黑帧误判为解码器失败。
4. 画质期望值：
   - Reset：`Video quality: 1080x1920, 120 fps, 12000 kbps`
   - Apply `1280×720,120fps,12000kbps`：`Video quality: 404x718, 120 fps, 12000 kbps`
5. 下一步优先做运动画面下的持续 120fps/12Mbps、弱网、真机/跨机；再补进程死亡授权与预测性 Back 自动化。
6. 继续平台缺口前先读需求原文；任何“通过”都必须带命令、环境、结果与证据路径。

---

## 8. 最终关键文件 SHA-256

- APK：`f5aad85a24fff0a12a7ca2b24e14441b5e435da66c3eb0c912d5bba02fa815de`
- `DfyH264Decoder.kt`：`3c675939f7ea96101b80a78ce2c08c4d9f509f11b3accc7dc768fda1bae9c9c9`
- `ViewerActivity.kt`：`25ad9b237d969460bb1273032688e75ac38b13bd5e2a300fac2e27d1164a7797`
- `RemoteService.kt`：`43921caf68846dce988db44664dac09c8518004d3c21a1acf42b7e44285da409`
- `MainActivity.kt`：`7d95fac38a55cbf10cbe22712256dbc494d1ea1242c06e8eb071b6a966140c00`
- `adaptation.rs`：`307b9bdb989ff92206abcbe7504878c597bbecc3961b1cabfa3b0cc0b1d2aef8`
- `agent.rs`：`0d4998d489b8e76c3801bf85484216d387a19000c092e1e2d83c03ec0275373f`
- `capture_loop.rs`：`d3a6a3da378f56cdbee0000f88ac92ffe4dea49f75cb25fdf34a6e5ba47121db`
- `migration.rs`：`89c472872ea87bca64d185d42e885c4dc7d5be2834e3ba532bbcc3617a5220cd`
- `tests/e2e.rs`：`7cdf371bd023e2520d187be4ac4195dc7e2598ba234f579bf8df7b26be6b4ff0`
- `resume.rs`：`d4cba4bda9428e7d182000da05743eeef4c249e984be40ce7673b683d82cccf3`
- `crates/dfy-android/src/lib.rs`：`84c3d176b27334c288311d35caeccf2fc8209cae60fe3ea49201f7667f1da5f6`

**一句话交接：最终包已通过独立复核、非黑 Viewer、画质 Apply/Reset 与硬件 Back；持续 120fps、弱网、真机/跨机、进程死亡授权和全平台功能仍未完成。**
