# 图形化管理面板（第一步，2026-11-27）

## 范围

- 新增 `crates/dfy-manager`（binary `dfy-manager`），用 winit 0.30 + softbuffer 0.4 开一个本地窗口，ab_glyph 0.2 在系统 TTF 上栅格化文本。
- 两个页签：
  - **Relay Profiles**：列出 `dfy-relay-profiles` 配置中的档案及 rtt/loss/bps 健康状态；↑↓ 或鼠标点击选择，T 测试选中，R 测试全部，U 设为当前，Tab/空格切换页签；
  - **Virtual Displays**：通过 `dfy-vdisplay::create_backend()?.list()` 列出 id、attached、几何、DPI 与名称，F5 刷新。
- 测试/选中使用与 CLI 相同的 `dfy-relay-profiles` 配置和原子保存；不发起远程会话、不改系统设置。
- 标签用 ASCII：Windows Arial 与 Linux DejaVu 都能栅格化；CJK 字形依赖系统字体，未做保证。

## 真实验收

- Windows Debug `dfy-manager.exe` SHA-256 `2a909189d91ae6179ea0d9b18404f139f524d3cd4b156354f8b413b35a83917d`。
- 实际启动：`dfy-manager.exe --config .tmp/relay-profile-test/profiles-gui.toml`，窗口标题 `Dafeiyu Remote - Manager`。
- 截图 `.tmp/manager-gui-final-20261127.png` SHA-256 `e17aaf3b95505fe749ef4e4857041b86fa389d61a4b6adb54186adf7455030e3`：可见页眉、两个页签、两行中继档案（local 显示 `rtt=Some(2.3085)ms loss=Some(0)ppm bps=Some(3840000)`，dead 显示未测试）、选中高亮与底部快捷键提示。
- 单测：`draw::tests` 3 项（混色、行几何、矩形裁剪）+ `manager_actions_write_health_and_selection`（对不可达档案测试失败写回、设为当前、重新加载持久化）。`cargo test -p dfy-manager`：4 通过 / 0 失败；strict clippy 通过。
- 修复：窗口尺寸小于 36px 时底栏 `height - 36` 曾触发减法溢出 panic，已改为 `saturating_sub`；同样保护列表行宽。

## 2026-11-27 续：字体回退、动作分派、虚拟屏操作

- 字体改为候选链：Windows 依次 `msyh.ttc → simhei.ttf → segoeui.ttf → arial.ttf`，Linux 依次 Noto CJK → 文泉驿 → DejaVu → Liberation；`--font` 显式指定仍优先。彻底没有可用字体时给出明确错误而不是 panic。
- 键盘事件先映射为与 winit 解耦的 `Command` 枚举，`apply` 执行动作；Quit 单独交给事件循环。新增单测 `command_mapping_is_deterministic` 覆盖全部按键映射。
- Displays 页新增 `C`（创建 1280×720/60Hz 预设虚拟屏）与 `D`（销毁选中虚拟屏），复用 `dfy-vdisplay` 后端；测试 `manager_actions_write_health_and_selection` 也覆盖切页、创建、销毁路径（无驱动时返回明确失败状态）。
- 最终 Windows Debug `dfy-manager.exe` SHA-256 `4d8f2dca79e0d0619c105bc1c2bc938cf396dd564afe9ea378b650eabbf1b4b9`；字体回退版真实起窗截图 `.tmp/manager-gui-fontfallback-20261127.png` SHA-256 `8961382a4a3ee23db5e5f4a429b148757d8252e72481a7e6e90d3323a9b8e106`（可见新的 C/D/F5 快捷键提示与两行档案）。
- `cargo test -p dfy-manager`：5 通过 / 0 失败；strict clippy 通过。

## 2026-11-27 续：Diagnostics 页

- 新增第三页签 `Diagnostics`（`G` 键或 `--tab diagnostics`）：对中继池与虚拟显示做只读一致性检查——id 唯一、端点非空、密钥可解析、健康状态时间/RTT、当前可选服务器、虚拟显示后端可用性。
- 启动参数 `--tab relay|displays|diagnostics` 便于无击键直达指定页签。
- 真实起窗截图 `.tmp/manager-diagnostics-20261127.png` SHA-256 `7f1220f00074866b4e08c457b64c8508f5aab569f1ae5f81935c2cac4bb46ae0`：第三页签高亮、两行 `OK`（`current relay: local`、`1 virtual display(s) listed`）、状态栏 `Diagnostics: 2 result(s)`。
- 单测新增：不一致健康状态被标记 FAIL、后端不可用被标记 WARN、`Command::Diagnostics` 填充页签；`cargo test -p dfy-manager` 7 通过 / 0 失败，strict clippy 通过。

## 边界

- 共享桌面环境下键盘焦点不稳定，T/R/U/C/D 的**真实击键路径未验收**；逻辑由单测覆盖，不把击键记为通过。
- 虚拟屏创建/销毁的 GUI 动作在 Windows IDD 未装机时返回后端失败；Linux Xvfb 的同类操作已由 CLI 独立验收，但 GUI 内未实测。
- 字体从系统路径读取，Windows 缺 Arial / Linux 缺 DejaVu 时窗口无法创建；未bundle 字体、未实现字体回退链。
- 只有 Relay 与 Displays 两个页签；没有会话拨号、质量统计可视化、主题/布局配置、分辨率自适应、无障碍支持。
- 不是设备发现/配对 GUI，也不替代 CLI；Linux/macOS 真机未验证。

## 2026-11-28 续：原生管理端与 Viewer 工具栏重构

以上各节记录 2026-11-27 的 winit/softbuffer/ab_glyph 原型，作为历史保留；当前 Manager 已换成 eframe/egui，旧绘制器 `src/draw.rs` 不再被入口引用。

### 实现范围

- 工作区起始 HEAD 为 `df627ecad0e4d5c2b361373ca2e5785cab7c8815`。改造前源码副本在 `.tmp/ui-rework-20261128/pre/{dfy-manager,dfy-ui}/`，清单 `.tmp/ui-rework-20261128/pre-sha256.txt` SHA-256 为 `5c2b44e74b1d68b7721448b070ae3aa6e053cca84de2aa47702147afba23a735`。
- `dfy-manager` 使用 eframe/egui 0.27.2 原生桌面部件，移除 winit/softbuffer/ab_glyph 直接依赖；rfd 0.15 提供系统文件对话框，zeroize 1 清理口令缓冲区。默认窗口 1120×720、最小 760×520，侧栏在逻辑宽度低于 900 时折叠为图标。
- 五页为设备、Agent、中继、虚拟显示、诊断。设备页有地址/口令直连、收藏、搜索、编辑/删除和指纹固定；Agent 页有绑定地址、权限、Relay、虚拟显示配置、随机口令和受控启停；中继页有四种策略、后台单点/全部探针、CRUD、导入与导出；显示页使用 typed `VirtualDisplayBackend` 并限制动态模式；诊断页显示可执行文件、配置、后端和子进程状态。
- 设备书签使用临时文件加 rename 原子保存，Unix 权限 0600，显式解析 TOML 并显示损坏/写入错误；不保存口令。会话口令限 16–1024 字节且不能全为空白，只通过子进程 stdin 传递，不进入 argv。Windows 子进程使用 `CREATE_NO_WINDOW`。Relay 连接只传档案 id/tag，密钥继续留在受保护配置中。
- Viewer 保留 winit/softbuffer 原生窗口和软件帧渲染，新增 `crates/dfy-ui/src/toolbar.rs`：全屏、暂停/恢复远控输入、八项统计浮层、断开；逻辑宽度自适应 32–56px 工具栏，最小逻辑客户区 640×360。全屏闲置 2 秒自动隐藏，F6 与方向键/Tab/Enter/空格提供键盘导航。
- 渲染与远端坐标映射共用客户区内容矩形；工具栏/统计浮层激活时隔离远端点击、键盘、按钮和滚轮。暂停、进入浮层、离开客户区、失焦和关闭都会释放已按下的远端输入。

### Windows 11 x64 实测

- Manager Debug 真实起窗，五页均以原生控件渲染且中文可读。新增设备弹窗通过实际键盘输入并保存，列表出现该书签，设备文件没有口令字段。
- Agent 页默认隐藏随机口令；本机 `127.0.0.1:28116` 启动后显示 running 和连接地址，Stop 后恢复未运行，测试 PID 5292 消失。观察到的子进程 argv 含 `agent --password-stdin --host 127.0.0.1 --port 28116 --allow-input`，不含口令。
- Windows IddCx 后端不可用时页面如实显示不可用，创建按钮禁用；没有把无驱动路径记成创建成功。
- Viewer 使用本机 WGC → 软件 H.264 → loopback QUIC，测试 Agent PID 19600、端口 28316。为安全起见 Viewer 未加 `--control`，因此本轮没有真实远控输入注入。
- 同一 Viewer 180 秒日志 `.tmp/ui-rework-20261128/viewer-ui-diagnostic.log`：21,318 DATAGRAM / 21.73 MiB、1,502 重组、1,491 解码、19 关键帧、0 解析失败、RTT 0.76ms、259 个质量反馈窗口、0 次路径迁移、最后帧 #2485/2485。
- 统计浮层已目视确认；临时动作日志直接记录 `ToggleInput`（逻辑坐标 128,44）和 `Fullscreen`（59,44）。全屏截图显示工具栏隐藏。临时诊断代码随后全部移除并重建最终二进制；自动隐藏的 2 秒边界由状态单测覆盖，不把截图外推成完整多屏几何验收。
- 测试 Agent PID 19600 已终止并等待退出，未保留本轮启动的 Agent/Viewer。

### 回归与产物

- `cargo fmt --all -- --check` 通过。
- `cargo test --locked -p dfy-manager -p dfy-ui`：Manager 7 项、Viewer 75 项、Viewer doctest 3 项，全部通过。Manager 旧原型相关测试随未引用绘制器退出，不把 9→7 解释为功能回归。
- 同两包 `cargo clippy --locked --all-targets -- -D warnings`：0 告警；`cargo build --locked -p dfy-cli -p dfy-manager` 成功。工具链为 Rust 1.100.0，不是 Rust 1.75 构建证明。
- 最终 `target/debug/dfy-manager.exe` SHA-256：`5f0fb8f83e39b68e3fa91c186eae865f911cee2765fc704761bd67bb3d5f6f52`。
- 最终 `target/debug/dfy.exe` SHA-256：`1d200e48e96c5818ca6dd6cc3124aa7c7bb266616138debf00ffec5b585ea986`。
- Viewer 诊断日志 SHA-256：`dae2fb67d2a4475fdc8bf87dace33a858957e6f208feb067079c914337c320fb`；Agent 日志 `.tmp/ui-rework-20261128/viewer-agent-smoke.log` SHA-256：`9dfee5b81c6fdec17b1658b0390372661310c7b44b00615d7a97785278ebdcc8`。
- SBOM/许可审计结果为 18 个 workspace crate、598 个第三方依赖，详见 `notes/ci-sbom-ui-release-20261127.md`。

### 仍未验收

- 同机 loopback 不是两台物理设备、公网、弱网或长稳验收；不能据此宣称达到 UU 或其它商业客户端的产品完成度。
- 尚未完成窄 Manager 窗口与多 DPI 的真实视觉验收、Relay GUI 全 CRUD/探针/导入导出、设备编辑/删除/收藏/搜索逐项点击、Manager 退出时活动子进程清理。
- 尚未通过新工具栏做真实远端输入注入，也未完成全屏/多显示器几何、各 OS 窗口、焦点/IME 和长期运行验收。
- 锁文件新增 244 个包版本；HEAD 基线已有依赖 MSRV 漂移，本次新增 ICU/IDNA 等包亦声明高于 1.75。本机没有 1.75 工具链，filter metadata 也不等于实际编译图，必须另做 1.75 解析/构建或明确调整工具链政策。
- 本次没有提交、推送、更新 tag/Release 或发布安装包。
