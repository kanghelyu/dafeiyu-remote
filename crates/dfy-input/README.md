# Linux X11 输入后端

`dfy_input::create_sink()` 在 Linux 中建立 X11/XTest 后端，公共 `InputSink` 和 `InputEvent` 接口不变。Windows 继续使用原来的 SendInput 实现。

- 当前进程需要有效的 `DISPLAY` 和 Xauthority 授权，服务器须提供 XKB 和 XTest。连接和扩展错误通过 `InputError::Unavailable` 报告。
- Wayland 会话与可识别的 XWayland 显示返回 `InputError::UnsupportedSession`。Wayland 支持需要单独实现用户授权的 RemoteDesktop portal。
- HID Keyboard/Keypad page `0x07` 根据服务器 XKB **物理键名**映射；不假定 evdev 偏移、不用字母 keysym 替代物理键。覆盖已有 Windows 表的键位，以及部分国际键和 Keyboard page 音量键；服务器缺少键位时返回 `UnsupportedEvent`。
- 目标端布局、快捷键、修饰键和 IME 处理物理按键。`modifiers` 位图只作诊断，不会补造按键。XTest 没有通用 Unicode/IME 文本提交接口，非空 `Text` 返回 `UnsupportedEvent`，不会修改全局键盘映射或自动访问剪贴板。
- 支持五个鼠标键、相对移动、按实时根窗口尺寸缩放的绝对移动。绝对坐标对应整个 X11 screen/root；本接口没有单显示器偏移字段。XTest 的有符号 16 位坐标不能表示超过 32768 像素的根窗口坐标轴，此时绝对移动明确报错。
- 滚轮 `120` 一格，支持双轴和正负方向，不足一格的增量会累计；`release_all` 清空余量。
- 按下时固定实际键码，`release_all`/`Drop` 释放已发送或可能部分发送的按键和按钮。释放失败保留记录供下一次调用重试；如果显示服务器已断开，仍只能尽力释放并记录错误。
- XKB 物理键码表在建立 sink 时读取。运行中修改符号布局不影响物理位置；重新分配 XKB 物理键码后应重建 sink。每个 sink 应由一个已授权会话独立拥有，多会话并发持有同一键的仲裁由上层负责。

## Linux 控制端观察

`dfy-cli` 的 Linux `view --control` 使用 XInput2 RawKey/RawButton/RawMotion 观察当前已授权 X11 会话，不向桌面注入。`x11_usage_to_physical_key_name` / `x11_physical_key_name_to_usage` 把 XKB 物理键名与 HID Usage 相互转换，使不同布局的主控端和被控端仍按物理位置通信。XKB 无法区分的共用物理键名采用固定规范用法；目标端布局/IME 仍由目标系统处理。

控制端只接受明确的 X11 会话，Wayland 与可识别 XWayland 返回错误，不绕过 RemoteDesktop portal 授权。鼠标移动按 10ms 合并，按钮/滚轮使用 HID 顺序；`Ctrl+Alt+Shift+Q` 停止采集前释放仍按住的键和鼠标键。

默认 `cargo test -p dfy-input` 只运行确定性映射、事件转换、失败恢复与契约测试，不连接 X11，不注入桌面输入。Linux 特定协议代码可用 `cargo check -p dfy-input --all-targets --target x86_64-unknown-linux-gnu` 交叉检查。

实际运行验证入口是 `tests/x11_inject.rs`，默认 `#[ignore]` 且要求 `DFY_INPUT_X11_TEST=1`。仅在**隔离、可销毁**的 Xvfb/Xorg 测试显示运行：

```sh
# 在单独终端启动；退出时结束这个测试进程。
Xvfb :99 -screen 0 1280x720x24 -nolisten tcp

DISPLAY=:99 XDG_SESSION_TYPE=x11 DFY_INPUT_X11_TEST=1 \
  cargo test -p dfy-input --test x11_inject -- --ignored --test-threads=1
```

此测试会真实注入输入。独立 X11 连接建立观察窗口，验证 KeyPress/Release、九种按钮事件（含四个滚轮方向）、QueryKeymap、QueryPointer 和 Drop 释放。它不证明真实桌面环境、多屏、多布局/IME、网络断线集成或长时间会话已通过；这些仍需平台联调矩阵记录实测结果。

Linux 控制端 XInput2 采集的显式隔离测试（会启动独立 Xvfb，并用 `xdotool` 产生真实 X11 事件）：

```sh
DFY_CLI_X11_CAPTURE_TEST=1 \
  cargo test -p dfy-cli --bin dfy \
  input_capture::linux::tests::isolated_x11_capture_observes_pointer_button_key_and_stop \
  -- --ignored --nocapture --test-threads=1
```

