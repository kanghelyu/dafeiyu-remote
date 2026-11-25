# 026 · Viewer UI（主控端「被控画面显示窗口」）

- 状态：MVP 实现（可用 + 可无头测试），CLI 集成待接
- crate：`crates/dfy-ui`
- 关联：`design/011-mvp-acceptance.md`（Ctrl+Alt+Del 转发、1080p60、<60ms）、
  `design/012-module-apis.md` §8（UI 用 winit + softbuffer，M1.5 换 wgpu）、
  `design/025-linux-capture.md`（帧的来源格式）、`design/021-clipboard-sync.md`（后续 UI 入口）

## 1. 目标与非目标

**目标**

1. 把远端的 BGRA/BGRx 解码帧显示到主控端窗口：等比缩放、居中、黑边（letterbox）；
2. 把窗口里的键鼠事件翻译成与 `dfy-input::InputEvent` 同语义的 `ViewerEvent`，
   交给上层转发到被控端；
3. 提供可替换的帧来源抽象，让 CLI `view` 子命令把「解码任务 → 窗口」接起来；
4. 纯逻辑（帧校验/缩放、键位映射、滚轮换算、统计、帧来源）**无显示环境也能单测**。

**非目标（本 crate 不做）**

- 不持有会话/网络/解码状态，不依赖 `dfy-session`/`dfy-transport`（只依赖 winit + softbuffer + tokio-sync）；
- 不做窗口管理（多窗口/标签页）、不做设置界面、不做剪贴板 UI；
- 不做色彩管理/HDR、不做硬件解码上屏（wgpu 外部纹理在 M1.5，见 §9）。

## 2. 对外 API

### 2.1 两个入口

```rust
/// 常驻模式：UI 跑在名为 "dfy-viewer-ui" 的独立 OS 线程，返回句柄。
pub fn spawn_viewer(config: ViewerConfig) -> anyhow::Result<ViewerHandle>;

/// 阻塞模式：在当前线程跑事件循环，直到窗口关闭/帧来源结束；事件丢弃。
pub fn run_viewer(config: ViewerConfig, frames: impl FrameSource) -> anyhow::Result<()>;

/// 阻塞模式 + 事件回调：每个 ViewerEvent 同步回调，返回 Close 可请求关窗。
pub fn run_viewer_with<E>(
    config: ViewerConfig,
    frames: impl FrameSource,
    on_event: E,
) -> anyhow::Result<()>
where
    E: FnMut(ViewerEvent) -> ViewerControl;
```

`spawn_viewer` 返回：

```rust
pub struct ViewerHandle {
    pub event_rx: tokio::sync::mpsc::UnboundedReceiver<ViewerEvent>,
    pub present: PresenterHandle, // Clone，可交给解码/网络线程
}
impl ViewerHandle {
    pub fn close(&self);
    pub fn try_recv(&mut self) -> Option<ViewerEvent>;
    pub async fn recv(&mut self) -> Option<ViewerEvent>;
}

pub struct PresenterHandle;
impl PresenterHandle {
    pub fn present(&self, width: u32, height: u32, bgra: Vec<u8>); // 校验失败 → warn 丢弃
    pub fn present_frame(&self, frame: Frame);                      // 已校验，推荐
    pub fn is_window_ready(&self) -> bool;
    pub fn close(&self);
}
```

### 2.2 配置

```rust
pub struct ViewerConfig {
    pub title: String,          // 窗口标题；show_stats 时追加统计
    pub initial_size: (u32, u32), // 初始客户区物理像素，默认 (1280, 720)；0 分量取默认，夹到 16..=16384
    pub fullscreen: bool,       // 启动即无边框全屏
    pub show_stats: bool,       // 标题栏显示「尺寸 · FPS · 单帧耗时」
    pub scale: Option<f64>,     // Some：首帧尺寸×scale 调整一次窗口（夹到 0.25..=4.0）；None：不动窗口
}
impl ViewerConfig {
    pub fn new(title: impl Into<String>) -> Self;
    pub fn with_initial_size(self, width: u32, height: u32) -> Self;
    pub fn with_fullscreen(self, fullscreen: bool) -> Self;
    pub fn with_stats(self, show_stats: bool) -> Self;
    pub fn with_scale(self, scale: Option<f64>) -> Self;
    pub fn effective_initial_size(&self) -> (u32, u32);
    pub fn effective_scale(&self) -> Option<f64>;
}
```

默认标题 `大肥鱼远程 · 主控端`；`fullscreen=false`、`show_stats=false`、`scale=None`。

### 2.3 事件

```rust
pub enum ViewerEvent {
    Key { usage: u16, down: bool, modifiers: u8 }, // HID page 0x07 usage + 8bit 修饰键位图
    Mouse { x: u32, y: u32, width: u32, height: u32,
            button: Option<u8>, down: bool },     // 远端画面像素坐标；button 1..=5 或 None=移动
    Wheel { dx: i16, dy: i16 },                   // 单位 WHEEL_DELTA=120，dy 正向上
    Closed,                                       // 终态：窗口已关/帧来源结束
    ToggleControl,                                // Ctrl+Alt+Shift+Q：切换「是否允许远控输入」
    ToggleFullscreen,                             // F11 已切换全屏（仅通知）
}
pub enum ViewerControl { Continue, Close }
```

语义与 `dfy-input::InputEvent`（`crates/dfy-input/src/event.rs`）逐项对齐：
`Key{usage,state,modifiers}`、`MouseButton`(1..=5)、`MouseMoveAbs{x,y,width,height}`、
`Wheel{dx,dy}`。

### 2.4 帧来源与帧

```rust
pub trait FrameSource: Send + 'static {
    fn try_next_frame(&mut self) -> FramePoll;
    fn poll_interval(&self) -> Duration; // 默认 8ms
}
pub enum FramePoll { Frame(Frame), Pending, Closed }
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(8);

pub struct Frame; // 校验通过的 BGRA 帧，width()/height()/size()/bgra()/into_bgra()/byte_len()
impl Frame {
    pub fn new(width: u32, height: u32, bgra: Vec<u8>) -> Result<Self, FrameError>;
    pub fn from_strided(width: u32, height: u32, stride: usize, data: &[u8]) -> Result<Self, FrameError>;
}
pub struct FrameIter<I>; // 迭代器 + 节流：new(iter) / with_interval(interval)
```

`FrameSource` 已为三种 channel 实现：tokio `UnboundedReceiver<Frame>`、tokio `Receiver<Frame>`、
`std::sync::mpsc::Receiver<Frame>`（空 → `Pending`，断开 → `Closed`）。

### 2.5 纯逻辑工具（可直接复用/单测）

- `frame`: `bgra_to_pixel([u8;4]) -> u32`（`0x00RRGGBB`）、`bgra_to_pixels`、
  `fit_rect(src_w,src_h,dst_w,dst_h) -> Option<Rect>`、`render_frame(src,src_w,src_h,dst,dst_w,dst_h) -> bool`、
  `cursor_to_frame(x,y,dst_w,dst_h,src_w,src_h) -> Option<(u32,u32)>`、`Rect`；
- `keymap`: `hid_usage(KeyCode) -> Option<u16>`、`modifier_bit(KeyCode) -> Option<u8>`、
  `hid_button(MouseButton) -> Option<u8>`；
- `scroll`: `WHEEL_DELTA`、`PIXELS_PER_LINE`、`WheelAccumulator`、`scroll_to_wheel`；
- `stats`: `RenderStats`（`record_render`/`fps`/`should_refresh_title`/`title`）、
  `FPS_WINDOW`(1s)、`TITLE_REFRESH_INTERVAL`(500ms)。

## 3. 窗口与渲染管线

### 3.1 状态机（winit 0.30 `ApplicationHandler`）

```
resumed ──┬─ 已有窗口：request_redraw 后返回（重复 resumed = 挂起恢复）
          └─ attach_window:
                create_window(title, initial_size)
                softbuffer::Context::new(window)
                softbuffer::Surface::new(&context, window)
                fullscreen? set_fullscreen(Borderless(None))
                sized_from_frame = fullscreen || scale.is_none()
                surface_size = (0, 0)   // 不要预填窗口尺寸：surface 尚无尺寸，
                                        // 留待首帧 render 真正 resize（见 §3.2 第 4 步）
                set_title(stats.title(...))
                mailbox.set_window_ready(); request_redraw()
user_event:
  Frame        → request_redraw
  FramesEnded  → emit(Closed) + exit
  Close        → emit(Closed) + exit
window_event:
  CloseRequested/Destroyed → emit(Closed) + exit
  Resized(_)               → surface_size = (0, 0)（标脏；下一帧 render 重新 resize surface）
                             + request_redraw
  RedrawRequested          → redraw_pending = false; render()
  KeyboardInput{is_synthetic:false} → on_keyboard
  CursorMoved → on_cursor_moved（先算远端画面坐标）
  MouseInput  → on_mouse_input
  MouseWheel  → on_wheel
  Focused(false) → modifiers = 0（避免远端「卡 Ctrl」）
about_to_wait: 事件通道已关闭 / exit_requested → exit
exiting:      surface = None; context = None; window = None
```

字段顺序即析构顺序（`surface`/`context` 在 `window` 之前），`exiting` 再显式置 `None` 兜底。

### 3.2 render()

1. `mailbox.take_latest()` 取走最新帧（没有新帧就复用 `last_frame`；连一帧都没有 → 纯黑）；
2. 首帧尺寸变化且 `sized_from_frame == false` → 按「帧尺寸 × `effective_scale()`」请求一次窗口尺寸
   （夹到 16..=16384，`scale` 夹到 0.25..=4.0）；全屏或 `scale=None` 时永不自动改窗；
3. 读 `window.inner_size()`；为 0（最小化/驱动未就绪）直接跳过；
4. `surface_size` 与当前 `inner_size` 不一致时 `surface.resize(NonZeroU32, NonZeroU32)`，成功才更新
   `surface_size`，失败 warn 并跳过本帧。**新建 surface、以及每次 `Resized` 都把 `surface_size`
   标脏为 `(0, 0)`**：softbuffer 的 surface 初始没有尺寸，若预填窗口尺寸而跳过 resize，
   `buffer_mut()` 会 panic「Must set size of surface before calling `buffer_mut()`」；
5. `surface.buffer_mut()` 拿 `&mut [u32]`：先 `fill(0)`（黑边/无帧都是纯黑），
   再 `frame::render_frame(...)` 把 BGRA 帧写进窗口缓冲区（最近邻，居中 letterbox）；
6. `buffer.present()` 上屏；失败 warn；
7. 上屏成功后 `RenderStats::record_render(now, frame_size, 耗时)`；
   `show_stats` 时每 ≥500ms 刷新一次标题（`{title} — {w}×{h} · {fps:.1} fps · {ms:.1} ms`）。

每帧整幅拷贝（softbuffer 的 buffer age/局部更新未用），空帧也会 present 一次清屏。

### 3.3 线程模型

| 形态 | 事件循环线程 | 推帧 | 收事件 |
| --- | --- | --- | --- |
| `spawn_viewer` | OS 线程 `dfy-viewer-ui` | `PresenterHandle`（任意线程，单槽最新帧 + `EventLoopProxy::send_event`） | tokio unbounded mpsc `event_rx` |
| `run_viewer(_with)` | 调用线程（CLI 主线程） | 后台线程 `dfy-viewer-frames` 轮询 `FrameSource`，写邮箱 + proxy 唤醒 | 同步回调 |

- 推帧永不阻塞：邮箱（`Mutex<Option<Frame>>`）只保留最新一帧，来不及渲染的旧帧被丢弃；
- `run_viewer` 退出时置 `stop: AtomicBool` 再 join 推帧线程（最多再等一个 `poll_interval`）；
- Windows 用 `EventLoopBuilderExtWindows::with_any_thread(true)`，所以 CLI 可以在 tokio
  `spawn_blocking` 里调 `run_viewer`；macOS 必须在主线程；Linux/X11 任意线程、Wayland 视合成器而定，稳妥仍放主线程；
- `PresenterHandle::close`/`ViewerHandle::drop` 只是发 `UiCommand::Close`，UI 线程异步退出，句柄侧不需等待。

## 4. 帧格式

- 来源：`dfy-session::DecodedFrame { width, height, stride, data: Vec<u8> /* BGRA8 */, pts_us }`
  （`crates/dfy-session/src/lib.rs:78-89`；`dfy-cli::real::bgra_to_ppm` 按 `data[i+2],data[i+1],data[i]` 输出 RGB PPM，可交叉验证字节序）；
- **内存字节序 `B, G, R, A`**（每像素 4 字节，alpha 在最高地址）；softbuffer 的像素是
  `u32 = 0x00RRGGBB`，`bgra_to_pixel` 做 `R<<16 | G<<8 | B`；
- **BGRx**（alpha 字节无意义）同样接受：alpha 完全不参与计算/显示，不做预乘；
- `stride != width*4` 时用 `Frame::from_strided`：按行拷贝去掉行尾 padding，
  期望长度 `(height-1)*stride + width*4`（最后一行可以没有 padding）；
- 校验规则（`Frame::new`/`Frame::from_strided` → `FrameError`）：

  | 情况 | 错误 |
  | --- | --- |
  | width==0 或 height==0 | `ZeroSize{width,height}` |
  | `width*height*4` 超 `u32`/`usize` | `TooLarge{width,height}` |
  | 连续帧 `bgra.len() != w*h*4` | `LengthMismatch{expected,actual}` |
  | stride < width*4 | `StrideTooSmall{stride,row_bytes}` |
  | stride 数据长度不足 | `LengthMismatch`（同上） |

- 非法帧**不会 panic**：`PresenterHandle::present` 打 warn 丢弃；`present_frame` 只接受已校验的 `Frame`；
- 帧尺寸可以在流中变化（重新编码/分辨率切换）：UI 会更新 `frame_size`，渲染始终按当前帧等比缩放；
- 每帧 `pts_us` 当前不用于显示调度（按到达即显示），用于统计/未来 A/V 同步（TODO）。

## 5. 缩放与色彩

**缩放**

- 显示策略：等比适应窗口 + 居中黑边（`fit_rect`，整数交叉相乘取整、四舍五入，宽高夹到 ≥1）；
- 放大/缩小都用**最近邻**（`render_frame` 的整数映射，放大时像素块状），保证 CPU 路径简单、无额外缓冲；
  双线性/硬件缩放列入 M1.5 wgpu 改造（§9）；
- 窗口缩放由用户拖拽/全屏控制（`Resized` → surface 重设尺寸 → 重绘）；`ViewerConfig::scale` 只影响**首帧**的一次自动定窗；
- **平移/自由缩放（鼠标拖拽、Ctrl+滚轮缩放画面）尚未实现**：当前总是整幅 fit。
  滚轮事件优先转给远端（滚动网页/文档是远控刚需），本地缩放需要单独的修饰键手势，列入 TODO（§9）。

**色彩**

- 不转换矩阵、不做 gamma/range 处理：解码器给的 BGRA 直接当 sRGB 显示（softbuffer 无色彩管理）；
- 若远端用有限范围（16..235）BT.709，主控端会出现轻微「发灰/黑位抬升」，色彩管理列 TODO；
- alpha 忽略（远端画面不透明），不做预乘/混合；HDR/10bit 不支持（帧格式按 8bit BGRA）；
- HiDPI：全程物理像素（`inner_size`、光标位置都用物理像素，`cursor_to_frame` 两端一致），
  Windows 的 DPI 缩放由 winit 处理，不额外乘 `scale_factor`。

## 6. 按键捕获与转发策略

### 6.1 捕获链路

```
winit WindowEvent::KeyboardInput { event, is_synthetic }
   is_synthetic = true（IME 合成）→ 整个事件忽略
   PhysicalKey::Code(code)（物理位置，不看布局/字符）
      ├─ keymap::modifier_bit(code) → 更新 self.modifiers（bit0=LCtrl … bit7=RGui）
      ├─ keymap::hid_usage(code)    → Some(usage) 则 emit Key{usage,down,modifiers}
      └─ 非 repeat 的按下 → 本地快捷键判断（见 6.2）
```

- **先更新修饰键位图，再发 Key 事件**：同一次按键里 `modifiers` 已包含它自己（按下的修饰键、或已释放的修饰键）；
- 位图与 `dfy-input::InputEvent::Key{modifiers}` 完全一致（bit0 LCtrl、bit1 LShift、bit2 LAlt、
  bit3 LGui、bit4 RCtrl、bit5 RShift、bit6 RAlt、bit7 RGui），可直接转发；
- HID usage 覆盖 dfy-input 能注入的范围：0x04..=0x73（含 0x64 Non-US `\`、0x65 Application、
  0x67、0x68..=0x73 F13-F24）与 0xE0..=0xE7（修饰键）；`IntlRo`/`IntlYen` 故意不映射（dfy-input 无对应注入表）；
- 键盘重复（`repeat=true`）：**照常转发**（远端按自己的重复率处理），但本地快捷键只在首次按下触发；
- 窗口失焦（`Focused(false)`）清空 `self.modifiers`，避免远端「卡住 Ctrl/Shift」（`dfy-input::PressedState` 是第二道防线）。

### 6.2 本地快捷键

| 组合 | 本地动作 | 原始按键是否转发 |
| --- | --- | --- |
| `F11` | 切换无边框全屏 + 发 `ToggleFullscreen` | 是（`F11` usage 0x45 先转发） |
| `Esc` | 发 `Closed`（终态）并关窗 | 是（`Escape` usage 0x29 先转发） |
| `Ctrl+Alt+Shift+Q`（左右修饰键均可） | 发 `ToggleControl`（切换「是否允许远控输入」） | 是（`Q` 照常转发） |
| 其它按键 | 无本地动作 | 是 |

快捷键**不吞事件**：远端仍收到原始键（例如 `Ctrl+Alt+Shift+Q` 会让远端也看到 Q 按下）。
`Ctrl+Alt+Del` 见 §9 的已知限制/CAD 转发策略。

### 6.3 鼠标

- 移动：`CursorMoved(position)` → `cursor_to_frame` 把窗口物理坐标映射成**远端画面像素坐标**
  （落在黑边上时夹到画面边缘，`min(src-1)`），缓存 `last_cursor_frame`，发 `Mouse{button:None}`；
- 按键：`MouseInput` → `hid_button`（Left=1、Right=2、Middle=3、Back=4、Forward=5；`Other` 忽略）→
  `Mouse{button:Some(n), down}`，坐标用最近一次移动的 `last_cursor_frame`（没有则 `(0,0)`）；
- 与 `dfy-input::InputEvent::MouseMoveAbs{x,y,width,height}` 语义一致（绝对坐标 + 画面尺寸，注入端自行换算）；
- 相对模式（FPS 游戏锁指针）未实现，列 TODO。

### 6.4 滚轮

- `LineDelta` 一格 = `WHEEL_DELTA=120`；`PixelDelta` 按 `PIXELS_PER_LINE=40px/行` 折算；
- winit 与协议符号相反（winit 正值表示内容向下/向右移动，协议 `dy` 正值向上、`dx` 正值向右），
  `scroll_to_wheel` 统一取反；不足一格的像素滚动留在 `WheelAccumulator` 里跨事件累计，不丢慢速滑动；
- 只发非零增量（`dx==0 && dy==0` 时不产生事件，避免触摸板海量空事件）。

## 7. 与 CLI（`view` 子命令）的集成点

现状：`crates/dfy-cli/src/real.rs:344 pub async fn run_view(ViewOptions) -> ViewStats` 目前只
打印统计、可存首帧 PPM（`bgra_to_ppm`），**还没有接 dfy-ui**；`crates/dfy-cli/Cargo.toml` 也还没有
`dfy-ui` 依赖。接法（不在本次改动范围内）：

```
dfy-cli view::run_view
  会话接收循环（tokio）
    DecodedFrame{width,height,stride,data:BGRA8}
        │ Frame::from_strided(width,height,stride,&data)   // 失败 warn 丢帧
        ▼
    tokio::sync::mpsc::UnboundedSender<Frame>
        │  接收端 UnboundedReceiver<Frame> 已实现 FrameSource
        ▼
    spawn_blocking || 主线程: run_viewer_with(ViewerConfig{...}, receiver, |event| { ... })
        │
        ├─ Key{usage,down,modifiers}  → InputEvent::Key{usage,state,modifiers}
        ├─ Mouse{...}                 → InputEvent::MouseButton / MouseMoveAbs
        ├─ Wheel{dx,dy}               → InputEvent::Wheel{dx,dy}
        ├─ ToggleControl              → 翻转「允许远控输入」开关（暂停/恢复注入）
        ├─ Closed                     → 结束 view（返回 ViewStats）
        └─ ToggleFullscreen           → 忽略（窗口已自己处理）
```

要点：

- `ViewerConfig`: `title` 可用远端地址/主机名，`initial_size` 取 CLI `--width/--height`（默认 1280×720），
  `fullscreen` 取 CLI 参数，`show_stats=true`（CLI 调试用），`scale=Some(1.0)` 让首帧定窗；
- 帧来源：解码任务把 `Frame` 发进 unbounded mpsc（解码慢/窗口快时自然丢弃积压？**不会**——unbounded
  会积压，CLI 侧应改用 `tokio::sync::mpsc::channel(N)` 有界 channel 并 `try_send` 丢弃旧帧；
  `Receiver<Frame>` 也实现了 `FrameSource`）；
- 输入按 `--control`/`--clipboard` 开关决定是否真的注入；`ToggleControl` 切换开关状态并在统计里显示；
- 退出：`Closed`（用户关窗/Esc/远端流结束）→ 停会话、打印 `ViewStats`；
  另需保留 `--seconds/--max-frames` 超时退出（现有逻辑）；
- `Ctrl+Alt+Del`：见 §9，CLI 需要一个显式入口（如 `Ctrl+Alt+End` 或菜单）而不是依赖本地捕获。

## 8. 无头可测的部分（feature 隔离）

`Cargo.toml`：

```toml
[features]
default = ["window"]
window = ["dep:softbuffer"]     # winit 保持非 optional（KeyCode/MouseScrollDelta 是纯逻辑 API 的类型）
```

- `window` 打开：`app`/`mailbox`/`run` + `spawn_viewer`/`run_viewer(_with)` + `PresenterHandle`/`ViewerHandle`；
- `--no-default-features`：只剩纯逻辑模块（frame/keymap/scroll/source/stats）+ 配置/事件类型，
  `cargo test -p dfy-ui --no-default-features --all-targets` 可在无显示环境跑（example 因 `required-features=["window"]` 自动跳过）；
- 不需要显示器/窗口系统的单测覆盖：帧校验与 strided 拷贝（`Frame`）、字节序、`fit_rect`/`render_frame`/`cursor_to_frame`、
  HID 映射与修饰键位、滚轮换算与累加器、统计（FPS 窗口/标题节流）、帧来源（三种 channel、`FrameIter` 节流、`pump_frames` 的停止/结束/唤醒失败路径）、
  mailbox（只留最新帧、并发写、window_ready）；
- winit/softbuffer 部分（事件循环、窗口创建、真实上屏）靠 `examples/viewer_demo -- --smoke`
  做 CI 冒烟（建窗成功即退出码 0），需要图形会话。

## 9. 待办 / 已知限制

**高优先级**

1. **CLI 集成**：`dfy-cli` 加 `dfy-ui` 依赖；`view` 子命令用 bounded mpsc + `run_viewer_with` 接解码帧；
   实现 `ViewerEvent → InputEvent` 翻译层（§7），保留 `--seconds/--max-frames` 超时退出；
2. **Ctrl+Alt+Del 转发**（`design/011` 验收项）：Windows 本地 `Ctrl+Alt+Del` 是 SAS，普通进程
   **永远收不到**（winit 也收不到），不能靠本地捕获。方案：a) 采用 RDP 惯例 `Ctrl+Alt+End`
   映射为远端的 `Ctrl+Alt+Del`（HID 顺序：LCtrl 0xE0 down、LAlt 0xE2 down、Delete 0x4C down/up、
   LAlt up、LCtrl up）；b) 或窗口菜单/按钮触发。需要 dfy-input 支持「多键序列注入」（当前一次只注入一个事件，
   `PressedState` 防卡键，但需要按序发 5 个事件且保证远端收到完整序列）；
3. 鼠标**相对模式** + `MouseMoveRel`（FPS/游戏/远程 CAD）；
4. 本地画面**平移/自由缩放**（修饰键 + 拖拽/滚轮，与远端滚轮冲突需约定）。

**中优先级**

5. wgpu 上屏（`design/012` §8 M1.5）：外部纹理直接显示硬解帧，省 CPU 拷贝、支持双线性/HDR 真正缩放；
6. 色彩管理：BT.709 有限范围 → 全范围/gamma 校正，10bit/HDR；
7. HiDPI 边界：目前按物理像素，Windows 混合 DPI 多显示器切换、macOS Retina 下的首帧定窗需验证；
8. 垂直同步/呈现节流、`Buffer::age` 增量更新（现在每帧整幅拷贝）；
9. 帧率/延迟面板（采集→解码→显示），以及 `pts_us` 用于 A/V 同步与迟到帧丢弃；
10. 文本输入（IME/Unicode）：当前只转发 HID 按键，`InputEvent::Text` 未接（中文输入法需要）；
11. 剪贴板/文件传输入口（`design/021`）、会话切换/多窗口。

**低优先级**

12. 统计标题 i18n、可配置快捷键、手柄；
13. 端到端自动化测试（现在只有 `--smoke` 建窗冒烟 + `--run` 阻塞模式演示）。

## 10. 测试与验证

```powershell
$env:Path = "$env:USERPROFILE\.cargo\bin;<mingw64>\bin;$env:Path"
cargo test -p dfy-ui --all-targets
cargo clippy -p dfy-ui --all-targets -- -D warnings
cargo fmt -p dfy-ui -- --check
cargo test -p dfy-ui --no-default-features --all-targets   # 证明无窗口 feature 可编译可测
cargo run -p dfy-ui --example viewer_demo -- --smoke       # 有图形会话时：真实建窗冒烟
```

覆盖矩阵（本次实现后的测试数量见 crate 报告）：帧校验/字节序/缩放/letterbox/光标反解、HID 与修饰键、
滚轮符号/累加/饱和、统计窗口与标题节流、帧来源 channel/节流/停止/关闭、mailbox 并发/最新帧、
配置夹取、示例帧生成器。