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
