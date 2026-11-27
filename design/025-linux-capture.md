# 025 · Linux 采集后端：X11 GetImage（v1）+ SHM / Wayland 路线

状态：**v1 已实现**（`crates/dfy-capture/src/linux.rs`，X11 `GetImage`，纯 Rust `x11rb`）；
v2（MIT-SHM / Damage）与 v3（Wayland）为本文设计，尚未落地。

相关：`design/012-module-apis.md`（crate 契约）、`design/020-virtual-display.md` §9（虚拟屏与采集的
id 映射、`recreate`）、`research/060-uu-linux-client.md` §3（UU Linux 采集管线的机制证据）与 §7
（借鉴 / 不照抄清单）。

---

## 1. 目标与非目标

### 1.1 目标

| # | 目标 | 落地 |
|---|---|---|
| G1 | Linux 上提供与 Windows 同形的采集 API：`new / monitors / capture`（+ `monitor / recreate / release / frames_acquired`） | `crates/dfy-capture/src/linux.rs` |
| G2 | 纯 Rust 实现，**不引入 C 依赖**（不链接 libX11/libxcb），协议层用 `x11rb` | `crates/dfy-capture/Cargo.toml` 的 `x11rb` target 依赖 |
| G3 | 多显示器枚举：RandR 1.5 `GetMonitors` → RandR 1.2 CRTC/output → Xinerama → 根窗口兜底，逐级降级 | `enumerate_geometries` |
| G4 | 抓帧输出 BGRA8（与 DXGI 一致，编码层无感知），正确处理 LSBFirst / MSBFirst、24/30/32 位深、scanline padding | `convert_to_bgra8` 等纯函数 |
| G5 | 不依赖真实 X server 的单测覆盖：BGRA 转换、几何解析/编号、DPI、stride 推断 | `linux.rs` 内 14 个 `#[cfg(test)]` 纯数据测试 |
| G6 | 纯 Wayland 会话（无 XWayland）给出可判别的 `CaptureError::Unsupported`，便于上层切 Wayland 后端 | `connect_display` 检查 `$DISPLAY` |
| G7 | Windows 编译与测试不受影响（新增代码全部 `cfg(target_os = "linux")`） | `cargo build/test -p dfy-capture` 全绿 |

### 1.2 非目标

- **Wayland 采集**（portal + PipeWire / Mutter 私有 D-Bus）：v3，见 §7；
- **增量脏区**：v1 全帧；v2 用 Damage 扩展补（§6）；
- MIT-SHM / DMA-BUF 零拷贝：v2/v3（§6、§7.3）；
- 输入注入（XTest / libei）、剪贴板、音频：属于 `dfy-input` / `dfy-clipboard` / 后续 crate；
- Linux 虚拟屏：属于 `dfy-vdisplay` 的 evdi 后端（`020` D3），本文只写采集侧集成点（§8.3）；
- Linux 编码后端（openh264/x264/VA-API）：属于 `dfy-codec`，目前该 crate 非 Windows 只有 stub，
  **这是 Linux 端到端远控当前的另一个硬缺口**（§8.4）。

### 1.3 决策摘要

| 决策 | 内容 | 理由 |
|---|---|---|
| D1 | X11 采集用 **`GetImage`（ZPixmap）** 而非 `ShmGetImage` 起步 | 无共享内存段生命周期、无 fd 传递、无 `unsafe`；一条请求即出图，最快能把 Linux 后端跑通；SHM 留作 v2（§6） |
| D2 | 协议栈用 **`x11rb`（RustConnection）** 而非 `xcb`/`Xlib` 绑定 | 纯 Rust 协议实现，不链接 C 库，符合「零 C 依赖」；`allow-unsafe-code` 只是 `RustConnection` 内部处理 fd 需要的 feature，**不会**引入对 libX11/libxcb 的链接 |
| D3 | 平台选择在 `lib.rs` 用三段 `cfg`：Windows → DXGI，Linux → X11，其余（macOS）→ stub | 保持已有 Windows 路径与 stub 语义不变，macOS 继续显式 `Unsupported` |
| D4 | `timeout_ms` **忽略**，每次 `capture` 同步抓一帧并返回 `Ok(Some)` | X11 `GetImage` 是同步请求/应答，没有「等新帧超时」这类事件语义；伪造 sleep 只会引入延迟假象（§4.5、§8.1 的限流问题单独处理） |
| D5 | 显示器枚举做成「**统一几何 → 编号/primary/DPI**」两段式纯函数 | 枚举来源（RandR 1.5/1.2、Xinerama、root）字段各不相同，先归一成 `MonitorGeometry`，让编号与 primary 兜底规则可以被单测覆盖 |
| D6 | 深度 24 时**强制 alpha = 255** | X11 depth=24 广泛以「每像素 32 位、高字节未定义」的形式存在（`bits_per_pixel=32`），高字节可能是 0 或垃圾，直接当 alpha 会让下游变透明 |

---

## 2. crate 结构与平台选择

```
crates/dfy-capture/
├── Cargo.toml            # + [target.'cfg(target_os = "linux")'.dependencies] x11rb
├── examples/capture.rs   # 仍是 Windows-only 示例（Linux 示例见 §10 待办）
└── src/
    ├── lib.rs            # 平台无关的类型 + 三段 cfg 选择后端
    ├── dxgi.rs           # Windows DXGI Desktop Duplication
    ├── linux.rs          # 新增：X11 GetImage 后端（含 14 个纯数据单测）
    └── stub.rs           # 仅 macOS / 其它平台
```

### 2.1 依赖（只对 Linux target 生效）

```toml
[target.'cfg(target_os = "linux")'.dependencies]
x11rb = { version = "0.13", features = [
    "randr",             # RandR 1.2/1.5：显示器枚举、布局变化
    "xinerama",          # Xinerama：RandR 不可用时的兜底
    "resource_manager",  # 连接时读 Xresource 里的 DPI 等提示（预留）
    "allow-unsafe-code", # RustConnection 用 libc 处理 fd；不链接任何 C 图形库
] }
```

解析结果（`Cargo.lock`）：`x11rb 0.13.2` / `x11rb-protocol 0.13.2` / `as-raw-xcb-connection 1.0.1` /
`gethostname 1.1.0` / `rustix 1.1.6` / `libc 0.2.191` / `linux-raw-sys 0.12.1` / `bitflags 2.13.2`。

要点：

- **不要**启用 `dl-libxcb`：那个 feature 会在运行期 `dlopen("libxcb.so")`，等于引入 C 依赖；
- `randr` 会隐式启用 `render`（RandR 协议里带 render 类型引用），但 `render` 不引入 C 库；
- `shm` 当前**未**启用（v1 不用 SHM），v2 打开它即可（§6）。

### 2.2 `lib.rs` 的平台选择

```rust
#[cfg(windows)]
mod dxgi;
#[cfg(windows)]
pub use dxgi::DxgiCapturer;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::LinuxCapturer;

// 非 Windows 非 Linux（目前只有 macOS）：保留原来的显式 Unsupported 语义。
#[cfg(not(any(windows, target_os = "linux")))]
mod stub;
#[cfg(not(any(windows, target_os = "linux")))]
pub use stub::DxgiCapturer;
```

即：**Windows 的 `DxgiCapturer` 一行未改**，Linux 新增 `LinuxCapturer`，macOS 继续走 stub。
`dfy-session` / `dfy-cli` 里针对 `DxgiCapturer` 的 `#[cfg(windows)]` impl 在 Linux 上不参与编译，
因此本 crate 的改动不会破坏其它 crate 的 Linux 编译（但也不会自动接上——见 §8.1）。

---

## 3. 显示器枚举

### 3.1 链路与降级顺序

`enumerate_geometries(conn, root, screen)` 按下列顺序尝试，**任何一层出错或返回空都静默降级**，
最终一定给出至少一个显示器（否则上层拿不到可采集目标）：

| 序 | 来源 | 协议 | 何时使用 | 名字 | mm/primary |
|---|---|---|---|---|---|
| 1 | `randr_get_monitors(root, true)` | RandR ≥ 1.5（查询 1.6，`minor_version >= 5`） | 现代 X server（xorg 1.20+） | monitor atom → `get_atom_name`，失败用 `monitor-{i}` | 有 mm、有 primary 标志 |
| 2 | `randr_get_screen_resources[_current]` + 逐 CRTC `get_crtc_info` + `get_output_info` | RandR ≥ 1.2 | 老 RandR / 1.5 返回空 | 该 CRTC 第一个 output 的 `name`，失败用 `CRTC-{crtc_id}` | mm 取 output；primary 用 `randr_get_output_primary`（RandR ≥ 1.3，1.2 上该请求报错即忽略） |
| 3 | `xinerama_query_screens()` | Xinerama | 无 RandR 的极老/精简 X | `Xinerama-{i}` | 无 mm、无 primary |
| 4 | 根窗口几何 | core | 以上全失败 | `default` | mm 取 `Screen.width_in_millimeters`，primary = true |

`GetScreenResourcesCurrent` 是 RandR **1.3** 才有的；1.2 服务器只有 `GetScreenResources`
（会把所有可能模式都枚举，略慢但兼容）。所以第 2 层按查询到的 `minor_version >= 3` 二选一。

**克隆模式（mirror / clone）去重**：多个 CRTC 可能指向同一块物理区域；第 2 层对
`(x, y, w, h)` 完全相同的几何做了去重，避免 `monitors()` 里出现重复屏（第 1 层 RandR 1.5 的
monitor 本身已是逻辑屏，天然去重）。0 尺寸、没有绑 output 的未激活 CRTC 直接跳过。

### 3.2 归一化与编号规则（可单测）

```rust
pub(crate) struct MonitorGeometry {
    name: String,
    x: i32, y: i32,          // 根窗口坐标系，可为负（主屏左侧的副屏）
    w: u32, h: u32,
    width_mm: u32, height_mm: u32,   // 0 = 未知
    primary: bool,
}

pub(crate) fn geometries_to_monitors(geoms: &[MonitorGeometry]) -> Vec<MonitorInfo>;
```

- **`id` = 枚举下标（0 起）**，与 `new(monitor_index)` 的入参一致。跨会话/热插拔后 id 可能漂移，
  上层若需要稳定标识应改用 `MonitorInfo.name`（RandR monitor atom / output 名相对稳定）；
- **primary 唯一化**：优先采用 RandR 标记的 primary；若没有任何一方标记 primary，退化为
  「坐标 (0,0) 起始的那块屏」；再没有就取第一块。最终**有且只有一个** `primary == true`
  （避免客户端按 primary 选屏时拿到多块）；
- **DPI**：`round(w_px * 25.4 / width_mm)`，任一维度缺失（0）按 **96** 处理，结果 clamp 到 1..=1000。
  X11 的 mm 字段经常是 EDID 或估算值，不保证精确——只用于 UI 缩放提示，不参与像素计算。

### 3.3 与「虚拟屏」的关系

X11 上有两种「多屏」形态，采集侧处理方式不同：

| 形态 | 根窗口尺寸 | 采集方式 | 说明 |
|---|---|---|---|
| **独立输出（RandR 逻辑屏）** | 各屏包围盒 | `GetImage(root, x, y, w, h)`，x/y 可为负 → 会被裁剪到根窗口内 | 主流形态；`monitors()` 逐屏给出矩形 |
| **扩展根窗口（X screen 变大）** | 比最大物理屏更大 | 同上，按几何取矩形 | Xvfb / RandR `set_screen_size` 造出来的「虚拟屏」属于这类（`research/060` §3.3：UU 的 Linux 虚拟屏就是 RandR 改屏，`xcb_randr_set_screen_size_checked`） |
| 单屏无 RandR | = 一个屏 | 兜底 `default` | 极简 X（某些容器 / Xvfb 初始状态） |

注意：

- 负坐标的显示器（主屏左侧的副屏）在根窗口内 `GetImage` 的 x 是**不允许为负**的，
  `capture_area()` 会把矩形饱和裁剪到 `[0, root_w) × [0, root_h)`，并把宽高夹到 ≥1；
  被裁掉的部分不在画面里（这种情况只在多屏包围盒大于根窗口时出现，属于异常拓扑）；
- **虚拟屏本身不是显示器**：`dfy-vdisplay` 在 Linux 上走 evdi（`020` D3）时，evdi 会创建真正的
  DRM connector，X server（或合成器）把它当作一个 RandR output，于是 `monitors()` 自然能看到它，
  采集侧不需要任何特殊分支——只需要 **id 映射**和**分辨率变化后 `recreate()`**（§8.3）。

## 4. 抓帧与像素格式

### 4.1 请求

```rust
conn.get_image(
    ImageFormat::Z_PIXMAP, // = 2，按像素取（不是位平面）
    self.root,             // drawable = 根窗口
    x, y,                  // i16，来自 display 矩形
    w, h,                  // u16
    u32::MAX,              // plane_mask = AllPlanes
)?.reply()
```

`GetImageReply { depth: u8, visual: Visualid, data: Vec<u8> }`。请求失败（`BadMatch` /
`BadValue` / 连接断）映射成带 `{w}x{h}+{x},{y}` 上下文的 `anyhow` 错误。

### 4.2 `depth` / `visual` → 像素布局

`X11Formats` 在 `new()` / `recreate()` 时从 `Setup` + 目标 `Screen` 预解析并缓存：

```rust
pub(crate) struct PixelLayout {
    bits_per_pixel: u8,   // 来自 pixmap_formats[depth].bits_per_pixel（常见 32 / 24 / 16 / 8）
    scanline_pad: u8,     // 行对齐位数（常见 8 / 16 / 32，下限按 8 处理）
    msb_first: bool,      // Setup.image_byte_order == MSB_FIRST
    red_mask: u32, green_mask: u32, blue_mask: u32,   // 来自 Visualtype
}
```

查找顺序（`X11Formats::layout`）：**reply 的 visual 精确匹配 → 根 visual → 同 depth 的任意 visual**。
全部 miss，或该 visual 三个掩码全 0（例如 PseudoColor / 索引色），返回 `None` → `capture` 报
「深度 N 没有可用的像素布局」。`bits_per_pixel` 缺失时按 32 兜底，>32 或 0 视为无效。

另外对回复字段做了防御：个别服务器/合成器对根窗口返回 `depth = 0` / `visual = 0`，这时回退到
`Setup` 里记录的 `root_depth` / `root_visual`，避免整帧失败。

### 4.3 stride（行跨距）

服务器返回的 `data` 长度是 `stride * height`，其中
`stride = ceil(width * bpp / scanline_pad) * scanline_pad / 8`（按**位**对齐后再转字节），
不能假定等于 `width * bpp / 8`。`pick_stride` 的推断顺序：

1. 理论 stride（上式）——只要 `理论stride * h <= data.len()` 就采用；
2. `data.len() / h`——要求 ≥ 最小行长（`ceil(width*bpp/8)`），兼容服务器多给的尾部 padding；
3. 理论 stride 能装下 `(w, h)` 时再试一次（前面第 1 步已覆盖，留作防御）。

推断失败 → 报错（附 `data.len()` / bpp），而不是越界读。

转换输出是**紧打包**的：`CapturedFrame.stride = width * 4`（BGRA8 每像素 4 字节），
与 DXGI 路径的约定一致，下游不需要处理 padding。

### 4.4 BGRA8 转换规则

```rust
pub(crate) fn convert_to_bgra8(
    src: &[u8], width: u32, height: u32, stride: usize, layout: &PixelLayout,
) -> Result<Vec<u8>, String>;
```

- **快路径**：`bpp == 32 && LSBFirst && 掩码为标准 0x00FF0000/0x0000FF00/0x000000FF`
  （即 x86 上最常见的 `BGRX`）→ 逐像素 memcpy 4 字节，只把第 4 字节（alpha）写成 255；
- **通用路径**：逐像素取 `bpp/8` 字节（LSBFirst 小端、MSBFirst 大端），分别用
  `red_mask/green_mask/blue_mask` 抽位，再按掩码宽度线性缩放到 8 位
  （`(raw * 255 + max/2) / max`）——所以 **10 bit / 深度 30（真彩 Deep Color）**、
  **16 bpp（RGB565）**、**BGR 顺序的服务器**都能正确出图；
- **alpha 一律 255**：X11 没有 per-pixel alpha 语义，depth 24/30 的高位字节未定义（D6）；
- 支持的 bpp 为 8/16/24/32；8bpp 也走通用路径（要求 visual 有掩码，即 TrueColor，索引色不支持）；
- 行尾 padding 只读不写，输出按 `width * 4` 紧打包。

单测覆盖（14 个，全部纯数据、不需要 X server）：

| 测试 | 验证点 |
|---|---|
| `converts_lsb_32bit_and_forces_opaque_alpha` | LSB 32bpp 快路径 + alpha 强制 255 |
| `converts_msb_first_32bit_with_byte_swapping` | MSBFirst 逐像素重排 |
| `converts_24bpp_with_scanline_padding` | 24bpp（3 字节/像素）且 stride 有 padding |
| `converts_10bit_masks_by_scaling` | 10 bit 掩码缩放到 8 bit（0x200/0x3FF → 128） |
| `rejects_short_or_misaligned_buffers` | 短缓冲 / 0 尺寸 / 行短于最小行长 → 报错 |
| `pick_stride_accepts_server_side_padding` | 服务器多给尾部 padding 时的 stride 推断 |
| `row_bytes_respects_scanline_pad` | 不同 `scanline_pad` 下的行字节计算 |
| `dpi_from_mm_rounds_and_falls_back` | DPI 四舍五入、mm=0 → 96、上限 clamp |
| `geometries_assign_ids_and_pick_primary_fallback` | id = 下标；无 primary 时取 (0,0) 屏 |
| `geometries_respect_explicit_primary_and_clear_duplicates` | 显式 primary 优先且唯一 |
| `randr_monitor_geometry_maps_fields_and_skips_disabled` | RandR 1.5 字段映射、0 尺寸跳过、负坐标 |
| `crtc_monitor_geometry_skips_disabled_crtc` | RandR 1.2 CRTC 映射与跳过规则 |
| `formats_standard` / `formats_layout_prefers_exact_visual_then_root_visual` / `formats_layout_returns_none_for_maskless_visual` | 布局查找的三级回退与索引色拒绝 |

### 4.5 `CapturedFrame` 的字段语义

| 字段 | Linux v1 的取值 | 说明 |
|---|---|---|
| `width/height` | 裁剪后的采集区尺寸 | 一般等于 `MonitorInfo.w/h` |
| `stride` | `width * 4` | 紧打包 BGRA8 |
| `format` | `PixelFormat::Bgra8` | 与 DXGI 一致，编码层无需分支 |
| `data` | `stride * height` 字节 | 每次抓帧新分配（v2 用 SHM 缓冲复用） |
| `timestamp_qpc` | 自采集器创建起的 `Instant::elapsed()` 微秒 | X11 没有 QPC 等价物；该字段只用于诊断与节流，**不参与时间轴**（编码时间轴由 `dfy-session` 的 `pts_us` 决定） |
| `dirty` | `[Rect::new(0, 0, w, h)]` | X11 无增量信息，整屏一个脏矩形 |
| `full_frame` | 恒 `true` | 同上；v2 接入 Damage 后才会出现 `false` |

- `timeout_ms`：**忽略**（D4）。`capture` 永远同步抓一帧并返回 `Ok(Some(_))`，请求失败返回 `Err`；
  调用方必须自己节流。这条与 DXGI「超时返回 `Ok(None)`」不同，是 Linux 后端与 `dfy-session`
  之间**唯一需要特别处理的语义差异**（§8.1）。
- `release()`：空操作（没有 Duplication 对象要释放），保留只为 API 对称；`Drop` 即关闭连接。
- `recreate()`：重新枚举显示器 + 刷新根窗口尺寸与像素布局表，保持 `monitor_index` 不变；
  索引已不存在 → `NoSuchMonitor`，调用方应重新 `new`。热插拔 / 改分辨率 / 改旋转后调用
  （对应 `020` §9 的「分辨率变化必须 recreate」）。

---

## 5. 已知限制（v1）

| 限制 | 影响 | 后续 |
|---|---|---|
| 纯 Wayland 会话无 `$DISPLAY` | `LinuxCapturer::new` 返回 `Unsupported`；**看不到任何画面**（XWayland 只让 X 客户端看到 X 窗口，看不到 GNOME Shell 自己的界面） | v3 Wayland（§7） |
| 每次 `GetImage` 都是「请求 → 服务器渲染/拷贝 → 整帧走 socket」的同步往返 | 1080p 单帧 8.3 MB，60 fps 约 0.5 GB/s 的内存/传输开销；比 SHM 慢一个量级（`research/060` §7.1 第 5 条） | v2 SHM（§6） |
| 无增量脏区 | 静止画面也整帧抓 + 整帧编码 | v2 Damage（§6） |
| 无光标合成 | X11 里光标是硬件 overlay，`GetImage` **不含**光标；客户端目前只能本地画 | v2 XFixes `get_cursor_image`（`research/060` §3.1：UU 就是这么做的） |
| 无 HDR / 色彩管理 | 只处理 8 bit 每通道（10 bit 会降到 8 bit），不支持 HDR 元数据 | 后续，随 `dfy-codec` 的 HDR 支持 |
| 索引色（PseudoColor，8bpp 调色板）不支持 | 极老/嵌入式 X 上 `layout()` 返回 `None` → 采集报错 | 需要读调色板（`GetColormapEntries`），低优先级 |
| 安全模型：X11 无按窗口的访问控制 | 任何能连上 X server（`DISPLAY` + `XAUTHORITY`/`~/.Xauthority`）的进程都能抓全屏；被控端以该用户身份运行时天然可抓 | 与 Windows 一致（同用户会话），不是新问题；Wayland 由 portal 授权 |
| 远端 X（`ssh -X`）不可用 | 服务器侧没有 MIT-SHM 且每帧走网络，实测不可接受的慢 | 明确不支持；需要远程场景请用 RDP/VNC 类协议 |
| 采集器未做 `Send` 断言 | 现状由 `dfy-session` 在专用采集线程内创建和使用（`start_capture_loop` 的 `build` 闭包），不跨线程移动 | 若要在多线程共享，需补编译期断言 |

---

## 6. v2 优化：MIT-SHM + Damage/XFixes（设计）

目标：把「整帧走 socket」变成「一次 X 请求 + 直接读本地共享内存」，并只处理变化区域。

### 6.1 MIT-SHM 抓帧

1. 开启 `x11rb` 的 `shm` feature（`Cargo.toml` 的 linux target 依赖里加 `"shm"`），协议不需要 C 库；
2. 建共享内存段：x11rb 的 `shm` 模块提供 `create_memfd`/`attach` 类封装（`research/060` §3.1 里
   UU 用的是 `memfd_create` + `ftruncate`，比 System V `shmget` 更干净、可跨进程传 fd，且不受
   `/dev/shm` 配额与残留段影响）；
3. `shm_attach(fd)` → `ShmSeg`，按 `ceil(w*h*bpp/8)` 调整段大小（改分辨率时 re-attach）；
4. 每帧 `shm_get_image(drawable=root, x, y, w, h, plane_mask=AllPlanes, format=ZPixmap, shmseg, offset=0)`
   → 应答只回 **size + 服务器写入的数据长度**，像素直接在共享内存里；
5. 像素转换（§4.4）可以**原地**做（BGRA 已是目标格式时直接零拷贝交给编码层），或双缓冲乒乓。

**风险 / 降级**：MIT-SHM 需要 X server 与客户端在同一台机器（`shm_query_version` 失败、
`BadAccess`、远程 X 都会失败）→ 必须保留 `GetImage` 路径作为 fallback，并把降级原因打日志
（`research/060` §7.1 第 4 条强调「每层都有 CPU 回退且会打日志说明降级原因」）。

### 6.2 增量脏区（Damage）

- `damage_create(widget, root, DamageReportLevel::NonEmpty)`，每个 X 事件循环迭代
  `damage_subtract` 取出并清零脏矩形，合并成 `CapturedFrame.dirty`，`full_frame = false`；
- 这需要把 `capture(timeout)` 从「立即抓一帧」改成「**等待** Damage 事件或超时」，
  于是 `timeout_ms` 才真正有意义（`Ok(None)` = 超时无变化）——与 DXGI 语义终于对齐；
- 事件循环会引入「读事件 + 抓帧」的时序问题：先 `damage_subtract` 再抓帧，保证不会漏掉
  抓帧期间的新变化（多抓一次无害，漏抓会花屏）；
- 需要 `SYNC` 扩展（`research/060` §3.1 的版本门槛 RandR ≥ 1.3、XFixes ≥ 2.0、SYNC ≥ 3.1）

### 6.3 光标

`xfixes_get_cursor_image` 取光标位图 + 位置，在 CPU 侧合成到帧上（或作为独立图层交给客户端）。
v1 不做，客户端本地画光标时会有位置延迟，但不会花屏。

### 6.4 预期收益（估算，未实测）

| 项 | v1 GetImage | v2 SHM + Damage |
|---|---|---|
| 每帧服务器侧 | 渲染 + 整帧 socket 传输 | 写入共享内存（无 socket 传输） |
| 客户端侧 | 读 socket + 转换 + 一次 full-frame memcpy | 直接读共享内存（可原地转换） |
| 静止画面 | 仍整帧抓 + 整帧编码 | Damage 无事件 → `Ok(None)`，零开销 |
| 典型收益 | 1× | 抓帧延迟降 1 个数量级，静止时 CPU 接近 0 |

---

## 7. v3 Wayland 路线（设计，未实现）

### 7.1 为什么不能复用 X11 路径

Wayland 下客户端**没有**「读任意屏幕」的能力：合成器只把画面交给被授权的一方。
XWayland 只让 X 客户端看到 X 窗口，看不到 GNOME Shell/KDE 自己的界面、看不到原生 Wayland 窗口，
因此「用 X11 后端凑合」在 Wayland 会话上是**明确不可用**的（`research/060` §3.3 的
`wayland_unsupported` 就是同一结论）。

### 7.2 通用路线：xdg-desktop-portal ScreenCast + PipeWire（长期方案）

1. D-Bus `org.freedesktop.portal.ScreenCast`：
   `CreateSession` → `SelectSources(types = MONITOR | WINDOW, multiple = true, cursor_mode = EMBEDDED|HIDDEN, restore_token)` → `Start` → 得到 `streams`（node id + 尺寸/位置）；
2. `OpenPipeWireRemote` 拿 remote fd → 用 PipeWire 客户端连该 fd，`pw_stream` 订阅 node；
3. 帧路径：DMA-BUF 优先（零拷贝，直接喂 VA-API/NVENC），不支持时 `renegotiating shared memory`
   走 CPU 缓冲（`research/060` §3.1(C) 的 `PipeWire no offerable DMA-BUF format, shared memory only`）；
4. 光标：`cursor_mode = EMBEDDED` 时合成器把光标画进帧（最省事）；`HIDDEN` 时自己用
   `PipeWire` 的 cursor meta 合成；
5. **恢复令牌**（`restore_token`）要持久化，否则每次连接都要用户点一次「共享屏幕」——这是
   portal 方案的产品体验关键点；
6. 输入：`org.freedesktop.portal.RemoteDesktop`（同样是 portal，配合 libei / EIS）；
7. 兼容性：GNOME / KDE / wlroots 系都实现 portal ScreenCast（版本差异大，需探测接口版本与
   source type 支持）。

### 7.3 GNOME 低延迟路线：Mutter 私有 D-Bus（UU 的做法，不建议照抄）

`research/060` §3.1(B) 的证据：UU 直接调
`org.gnome.Mutter.RemoteDesktop(.Session)` / `org.gnome.Mutter.ScreenCast(.Session)`
（对象路径 `/org/gnome/Mutter/RemoteDesktop`、`/org/gnome/Mutter/ScreenCast`，信号
`PipeWireStreamAdded`，光标单独一个 PipeWire 节点 `nrd-wayland-cursor`），**绕过 portal 的
交互确认**，延迟更低，但：

- 只支持 GNOME/Mutter，且强绑 GNOME 版本（`research/060` §7.2 第 2 条：GNOME 46 专用扩展
  `shell-version:["46"]` 就是版本敏感的证据）；
- 接口随时可能变，非公开契约。

**结论**：我们**不**把 Mutter 私有接口作为主路径；可以作为 GNOME 上「portal 不可用/延迟不满意」
的可选加速路径，用 feature flag 隔离，且必须能在失效时自动回落到 portal。

### 7.4 与现有抽象的衔接

新增 `LinuxCapturer::new` 失败（`$DISPLAY` 缺失 / 连不上）时的**探测顺序**建议：

```
X11（本文件，已实现）──失败且错误是 Unsupported──▶ Wayland（portal，v3）──失败──▶ 显式报错给 UI
```

API 上保持同一组方法（`new/monitors/capture/monitor/recreate/release`），由
`dfy-capture` 内部按 `WAYLAND_DISPLAY`/`DISPLAY` 选后端，上层（`dfy-session`）无需改代码；
但 `capture(timeout)` 的语义在 Wayland 下是「等 PipeWire 新帧或超时」，与 v2 的 X11 SHM 路径一致。

---

## 8. 与其它 crate 的集成点（**本次未改，需后续提交**）

### 8.1 `dfy-session`：`Capturer` impl + **忙轮询问题（重要）**

需要新增（与现有 Windows 版并列）：

```rust
#[cfg(target_os = "linux")]
impl Capturer for dfy_capture::LinuxCapturer {
    fn capture(&mut self, timeout_ms: u32) -> anyhow::Result<Option<RawFrame>> {
        let frame = dfy_capture::LinuxCapturer::capture(self, timeout_ms)?;
        Ok(frame.map(|f| RawFrame {
            width: f.width,
            height: f.height,
            stride: f.stride,
            data: f.data,
            pts_us: dfy_transport::now_us(),   // 与 Windows impl 一样用墙钟微秒
            full_frame: f.full_frame,
        }))
    }
}
```

**忙轮询问题**：`capture_loop.rs` 的循环在 `Ok(Some(frame))` 之后如果未到 `max_fps` 间隔就
`continue`，**不 sleep**：

```rust
if now.duration_since(last_emit) < min_interval {
    continue;   // ← DXGI 路径没事：下一次 capture 会在 DXGI 里阻塞到超时；
}               //    Linux 路径每次立刻返回 Some → 变成全速 GetImage 空转
```

Linux 上每次 `capture` 都会真的抓一帧（整帧 memcpy + BGRA 转换，1080p 约 8 MB），
按默认 `max_fps = 60`、`capture_timeout_ms = 16` 推算，空转会以数百 fps 抓帧并丢弃，
白白烧 1 个核和大量内存带宽。**上线 Linux 采集前必须处理**，两个方案：

1. 改 `capture_loop.rs`（推荐，一处 patch）：丢弃前 `thread::sleep(min_interval - elapsed)`；
   顺带让 DXGI 路径的限流更准；
2. 或在 `LinuxCapturer::capture` 内部自限速（记录上次返回时刻，距上次不足最小间隔就
   sleep 到点）——但这会让 `timeout_ms` 的语义更混乱，不如方案 1。

另外 `dfy-session` 的 `start_capture_loop` 在**独立线程**里跑 `build()`，因此 Linux 连接
（x11rb socket）在采集线程内创建即可，不涉及跨线程移动。

### 8.2 `dfy-cli`：`real.rs` 的 Linux 分支

现状（`crates/dfy-cli/src/real.rs:79-83`）非 Windows 直接 `bail!("当前平台的采集/编码后端尚未实现")`。
接 Linux 采集时要把 `#[cfg(not(windows))]` 拆成
`#[cfg(not(any(windows, target_os = "linux")))]` + `#[cfg(target_os = "linux")]`，
Linux 分支用 `LinuxCapturer::monitors()` / `monitor_index` 选屏、在 `start_capture_loop` 的
`build` 闭包里 `LinuxCapturer::new(...)`。**注意**：即使采集接上，编码仍是 stub（§8.4），
所以这条路径现在接上也只能跑「采集 → 丢弃」，要等 Linux 编码后端。

### 8.3 `dfy-vdisplay`（Linux = evdi，`020` D3）

- evdi 创建的是真正的 DRM connector，被 X server 识别为普通 RandR output →
  `LinuxCapturer::monitors()` 自然包含它，**采集侧零改动**；
- **id 映射**（`020` §9）：`dfy-vdisplay` 的 display id 与 `dfy-capture` 的枚举下标**不是同一个
  编号**，必须在 agent 层按名字/位置映射；X11 下 `MonitorInfo.name`（output 名，如
  `Virtual-1`、`eDP-1`）比 id 稳定，建议用名字做映射键；
- 虚拟屏 `update()` 改分辨率后：调用 `LinuxCapturer::recreate()` 刷新根窗口尺寸与几何，
  再由 `dfy-session` 重建编码器并强制关键帧（否则客户端花屏到下一个 I 帧）；
- 虚拟屏可能是**唯一**显示：evdi connector 存在但未启用时 `monitors()` 可能看不到它 →
  agent 需要在 `create` 后等待 X server 完成热插拔（轮询 `monitors()` 直到出现，带上限）。

### 8.4 `dfy-codec`：Linux 编码缺口（端到端阻塞项）

`crates/dfy-codec` 目前非 Windows 是 stub（`create_encoder` 返回「当前平台不支持」）。
Linux 端到端要能推流，还需要一路 H.264 编码后端，候选（按 `research/060` §3.2 的取舍）：

| 后端 | 优点 | 代价 |
|---|---|---|
| **OpenH264**（Cisco，BSD） | 纯软件、稳定、源码可控；UU 也把 OpenH264 当兜底 | 需要 C 编译（或预编译 so），与我们「零 C 依赖」的偏好冲突；性能只够 1080p 中低帧率 |
| **x264**（GPL） | 质量/性能好 | GPL 传染性与本仓 PolyForm Noncommercial 1.0.0 的分发条款兼容需法务确认 |
| **VA-API**（libva，MIT） | 硬编，零拷贝（DMA-BUF） | 需要 `libva` dlopen + 按 GPU 选编码入口（H.264/H.265），实现成本最高 |
| **NVENC**（dlopen `libnvidia-encode.so.1`） | N 卡上延迟最低、零拷贝（Vulkan→CUDA 外部内存） | NVIDIA 专有驱动，需运行期探测 |

建议顺序：先 **OpenH264 静态/动态链接**跑通端到端，再补 VA-API/NVENC。
（Linux 上 dlopen 系统库属于运行时可选依赖，是否算「C 依赖」需要与业主确认策略；
`dfy-capture` 本身仍然零 C 依赖。）

### 8.5 `dfy-input`（后续）

X11 输入注入走 XTest（`XTestFakeKeyEvent/ButtonEvent/MotionEvent`，`research/060` §4.1），
同样可用 `x11rb` 的 `xtest` feature 纯 Rust 实现；Wayland 走 libei / portal RemoteDesktop。
不在本次范围。

---

## 9. 容量估算（估算，未实测）

| 分辨率 | 单帧 BGRA8 | 60 fps 带宽（抓帧→编码） | 备注 |
|---|---|---|---|
| 1920×1080 | 8.29 MB | ≈ 0.50 GB/s | 目标最低档 |
| 2560×1440 | 14.7 MB | ≈ 0.88 GB/s | |
| 3840×2160 | 33.2 MB | ≈ 1.99 GB/s | v1 GetImage 路径下 socket+memcpy 会成为瓶颈，建议 v2 前只保证 1080p/1440p |

编码侧 `dfy-codec` 要求输入 NV12（见 `design/012-module-apis.md`），BGRA8 → NV12 还要一次
全帧色彩转换；v2 的 SHM/DMA-BUF 路径应尽量把「抓帧 → 转换 → 编码」串成零拷贝链路。

---

## 10. 验证与待办

### 10.1 本次验证

| 项 | 命令 | 结果 |
|---|---|---|
| Windows 构建 | `cargo build -p dfy-capture` | ✅ `Finished dev profile in 1m 22s`（新增代码在 Windows target 不参与编译） |
| Windows 单测 | `cargo test -p dfy-capture` | ✅ 4 passed / 1 ignored（`captures_a_real_frame` 需真实桌面会话，原样保留） |
| Linux 交叉检查 | `cargo check -p dfy-capture --target x86_64-unknown-linux-gnu` | ✅ 通过（`rustup target add x86_64-unknown-linux-gnu` 后；无警告） |
| Linux 交叉检查（含测试/示例） | `cargo check -p dfy-capture --target x86_64-unknown-linux-gnu --all-targets` | ✅ 通过（说明 `#[cfg(test)]` 的 14 个测试、example 在 Linux target 下都能编译） |

`cargo check --target` 只能验证**编译**，不能执行 Linux 二进制（本机无 WSL/Linux 运行时），
所以 `linux.rs` 的 14 个单测是「编译通过但未运行」。

### 10.2 待办（按优先级）

1. **（阻塞项）** 真实 X11 机器上跑通 `LinuxCapturer::new/monitors/capture`，把抓到的帧写成 PNG
   人工核对（颜色通道顺序、alpha、多屏裁剪、负坐标副屏）；
2. **（阻塞项）** `dfy-session` 的入口 impl + `capture_loop` 限流 patch（§8.1），否则空转烧 CPU；
3. `dfy-codec` Linux 编码后端（§8.4），否则端到端无法推流；
4. Linux 示例程序（`examples/capture_linux.rs`，`cfg(target_os="linux")`）替代 Windows-only 的
   `examples/capture.rs`，方便真机验证；
5. v2 SHM + Damage（§6）；v3 Wayland portal（§7）；
6. 多显示器实测：克隆模式、混合 DPI、2×2 拼屏、Xvfb/Xwayland 边界；
7. 每帧分配优化：v1 每次 `capture` 都新分配 `Vec<u8>`，可以用复用缓冲（`Option<Vec<u8>>` 池）。

---

## 11. 参考

- `design/012-module-apis.md`：`dfy-capture` 契约（注意：该文档的 `MonitorInfo`/`CapturedFrame`
  字段描述落后于实际代码，以 `crates/dfy-capture/src/lib.rs` 为准）
- `design/020-virtual-display.md`：虚拟屏跨平台方案（D3 = Linux evdi）、§9 集成点
- `research/060-uu-linux-client.md`：
  - §3.1 X11 分支（MIT-SHM + memfd、Damage/XFixes/RandR/SYNC、版本门槛 RandR≥1.3/XFixes≥2.0/SYNC≥3.1）
  - §3.1(B) Wayland（Mutter 私有 D-Bus、`nrd-wayland-cursor` 独立光标 PipeWire 节点）
  - §3.1(C) PipeWire 双路径（DMA-BUF 优先，`renegotiating shared memory` 回退）
  - §3.3 Linux 虚拟屏 = RandR 改屏，Wayland 明确不支持
  - §4.1 X11 = XTest 注入；Wayland = Mutter `NotifyKeyboardKeycode`
  - §7.1 第 5 条（SHM 比 XGetImage 快一个量级）、§7.2 第 2/3 条（不照抄 Mutter 私有接口与 RandR 改屏）
- x11rb 0.13.2：`RustConnection`（纯 Rust 协议栈）、`randr` / `xinerama` / `shm` 扩展模块