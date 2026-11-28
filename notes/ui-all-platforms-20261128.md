# 大肥鱼远程全平台 UI 打磨（2026-11-28）

## 范围与基线

- 在远端 macOS 更新 `f55cbfb` 与本地原生 Manager/Viewer 重构 `ed8b8e0` 之上继续打磨。
- 统一中文品牌“大肥鱼远程”；设备书签不保存密码，口令只经 stdin 传给子进程并 Zeroizing 释放。
- Relay 没有图形界面，本轮只保留其既有 loopback HTTP 健康/指标接口，不新增未要求的控制台。

## 桌面 Manager

- `eframe 0.27.2` / `egui 0.27.2` 原生窗口，深海军蓝与薄荷绿视觉，重新整理设备、Agent、中继、显示器和诊断五页。
- 卡片、导航、按钮、状态、空态、加载、错误、进度与表单校验统一；窄窗口自动切换紧凑导航，最小宽度 680px。
- Windows 真实起窗目视检查 1122×752 与 682×752 五页及添加设备校验弹窗。
- 无 Dafeiyu/IddCx 虚拟显示后端，创建和已填充虚拟屏状态未在本机触发。

## 桌面 Viewer

- `winit 0.30` + `softbuffer` 原生像素渲染，保留 letterbox、DPI 缩放、帧邮箱和输入隔离。
- 悬浮工具条提供全屏、暂停/继续、连接统计、断开；支持 Tab 导航、自动隐藏、悬停/按下/危险态和焦点提示。
- 1080 逻辑像素以下切换紧凑文案，窄窗口仍保留全部四个操作。
- 内嵌按需子集化并改名的 Noto Sans SC 字体，随仓库提供 OFL 许可；字形栅格缓存、抗锯齿、中文省略和圆角面板均由原生代码绘制。
- Windows `viewer_demo` 宽窗口实测：中文工具条、统计卡、暂停/继续状态与底部提示均正确；706×428 窄窗口实测统计卡、暂停快捷键和按钮点击均正确。先前窄窗口点击失败是测试点落在标题栏，校准截图坐标后通过。
- 未做最终高 DPI 屏幕实测、macOS/Linux 新 UI 目视验收或真实跨机控制。

## Android 与平台专属界面

- Android Host/Viewer 改为程序化原生视图：统一主题、卡片、分区、状态、表单、加载/错误/空态和底部会话操作，不引入 Compose。
- 密码输入禁止保存视图状态；暂停只释放输入与回调，MediaCodec 继续在 `onDestroy` 或明确断开路径释放，避免主线程 `onStop` 同步等待解码线程。
- JDK 17 执行 `:app:testDebugUnitTest :app:lintDebug :app:assembleDebug --offline -x cargoBuild`：35 测试通过，lint 0 错误/25 警告，Debug APK 构建成功。APK SHA-256 `95c2d382d2dad92ae69b0559583e453fbee66f4de1c6d760c49b3efe5e657c36`。
- 未取得可用模拟器/真机的新 UI 目视验收；复用既有 native 库，不代表重新验证采集、编解码、旋转或真实输入。
- Linux desktop entry 显示“大肥鱼远程”，无终端启动 `dfy-manager`；DEB/RPM 和 Release workflow 补齐 Linux Manager 二进制与 GUI 运行依赖。
- macOS `.app` 的 `CFBundleDisplayName` / `CFBundleName` 和 DMG 卷名使用“大肥鱼远程”；保留与 plist/打包路径一致的 `Dafeiyu Remote.app` 文件系统名称。

## 最终本地验证

- `cargo fmt --all -- --check`、`cargo check --locked -p dfy-manager -p dfy-ui`、两包 `cargo test --locked`、两包 strict Clippy 全部通过：Manager 8 测试，Viewer 78 测试 + 3 doctest。
- `cargo check --locked -p dfy-ui --no-default-features` 通过，仍有 toolbar-only 配置既有的 24 条 dead-code 警告。
- `MSYS=winsymlinks:nativestrict python packaging/tests/smoke.py`：所有宿主机可运行打包检查通过；未构建真实 Linux DEB/RPM 或 macOS PKG/DMG。
- `python packaging/generate_sbom.py --check`：18 workspace crates + 598 第三方依赖审计通过。
- `python docs/requirements/split-and-verify.py --check`：825 行、28 模块、0 缺失/重复、字节拼接一致，PASS。
- 仅安装 Rust 1.100；未在 Rust 1.75 实际构建，新增依赖的 MSRV 声明仍是明确边界。
