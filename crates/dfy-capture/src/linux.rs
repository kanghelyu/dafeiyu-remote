//! Linux / X11 屏幕采集后端。
//!
//! # 路线
//!
//! * 用纯 Rust 的 [`x11rb`]（**不**链接 `libX11` / `libxcb`）直连 `$DISPLAY`；
//! * 显示器枚举按 RandR 1.5 `GetMonitors` → RandR 1.2 CRTC/Output → Xinerama →
//!   根窗口单屏，四层逐级降级，任何一层失败都不会让枚举整体失败；
//! * 抓帧用 X11 核心协议 `GetImage(ZPixmap, root, x, y, w, h, AllPlanes)`，
//!   再按服务器字节序 + visual 掩码转成 BGRA8（与 Windows DXGI 后端一致）。
//!
//! # 为什么 v1 不用 MIT-SHM
//!
//! `GetImage` 的像素由 X 服务器通过 socket 回传，每次抓帧都是一次同步往返，
//! 带宽和 CPU 都远高于「共享内存 + 服务器直接写」的 MIT-SHM 路线。
//! 但 SHM 需要 `x11rb` 的 `shm` 特性、服务器端共享内存段与 fd 传递（memfd /
//! `shm_open`），还牵涉 X 服务器对 fd 传递的支持，v1 先保证「能采、采对」，
//! SHM 与 Damage 增量放在 v2。详见 `design/025-linux-capture.md`。
//!
//! # 已知限制
//!
//! * `timeout_ms` 被忽略：X11 没有「等待新帧」的机制，每次调用立即抓一帧并返回
//!   `Ok(Some(..))`，节流由上层负责；
//! * 没有增量脏区信息：`dirty` 恒为整个采集区、`full_frame` 恒为 `true`；
//! * 只支持「根窗口覆盖全部输出」的单 X screen 模型（现代桌面即此模型）；
//!   远程桌面虚拟屏（evdi / RandR 扩展根窗口）需要上层在扩展根窗口之后再枚举；
//! * Wayland 会话（无 XWayland）没有 X 根窗口，本后端不可用；XWayland 下根窗口
//!   也不包含 Wayland 原生窗口的内容，因此只适合 X11 桌面会话；
//! * 显示器布局/分辨率变化后需要 [`LinuxCapturer::recreate`]（与
//!   `design/020-virtual-display.md` §9 的约定一致）。

use std::time::Instant;

use anyhow::anyhow;
use x11rb::connection::{Connection as _, RequestConnection as _};
use x11rb::protocol::randr;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xinerama;
use x11rb::protocol::xinerama::ConnectionExt as _;
use x11rb::protocol::xproto::{Atom, ConnectionExt as _, ImageFormat, Screen, Setup, Window};
use x11rb::rust_connection::RustConnection;

use crate::{CaptureError, CapturedFrame, MonitorInfo, PixelFormat, Rect};

/// 每像素最大支持 32 位；更宽的视觉（如 64 位深色）不在 v1 范围内。
const MAX_BITS_PER_PIXEL: u8 = 32;

/// X11 后端的采集器（一个实例持有一条 X11 连接）。
///
/// 与 Windows 的 `DxgiCapturer` 暴露同一组方法：`new` / `monitors` /
/// `capture`，外加 `monitor` / `frames_acquired` / `release` / `recreate`
/// 用于诊断与重建。
pub struct LinuxCapturer {
    /// X11 连接（由 x11rb 纯 Rust 协议栈实现，含自己的 socket）。
    conn: RustConnection,
    /// 连接建立时选中的 screen 序号（多 screen 的 X 服务器上才有意义）。
    screen_num: usize,
    /// 根窗口，`GetImage` 的 drawable。
    root: Window,
    /// 根窗口（= 整个虚拟桌面）的像素尺寸，用于把显示器矩形裁剪到合法范围。
    root_width: u16,
    root_height: u16,
    /// 创建时选中的显示器序号（`recreate` 时复用）。
    monitor_index: u32,
    /// 当前采集的显示器。
    monitor: MonitorInfo,
    /// 像素布局表（深度 / visual 掩码 / 每像素位数 / 扫描线对齐）。
    formats: X11Formats,
    /// 采集器创建时刻，用于给每帧打单调递增的时间戳。
    started: Instant,
    /// 成功采集的帧数（诊断用）。
    frames_acquired: u64,
}

impl LinuxCapturer {
    /// 枚举当前 X11 会话上的显示器。
    ///
    /// 返回值顺序与 [`LinuxCapturer::new`] 的 `monitor_index` 一致。
    pub fn monitors() -> anyhow::Result<Vec<MonitorInfo>> {
        let (conn, screen_num) = connect_display()?;
        let geoms = {
            let screen = &conn.setup().roots[screen_num];
            enumerate_geometries(&conn, screen.root, screen)?
        };
        if geoms.is_empty() {
            return Err(anyhow!("X11: 没有枚举到任何显示器"));
        }
        Ok(geometries_to_monitors(&geoms))
    }

    /// 为 `monitor_index` 指定的显示器创建采集器。
    ///
    /// 索引越界返回 [`CaptureError::NoSuchMonitor`]；`$DISPLAY` 缺失时返回
    /// [`CaptureError::Unsupported`]（通常意味着当前会话是纯 Wayland）。
    pub fn new(monitor_index: u32) -> anyhow::Result<Self> {
        let (conn, screen_num) = connect_display()?;

        let geoms = {
            let screen = &conn.setup().roots[screen_num];
            enumerate_geometries(&conn, screen.root, screen)?
        };
        let monitors = geometries_to_monitors(&geoms);
        let monitor = monitors
            .into_iter()
            .nth(monitor_index as usize)
            .ok_or(CaptureError::NoSuchMonitor(monitor_index))?;

        let formats = {
            let setup = conn.setup();
            // screen_num 已由 connect 返回，必然在 roots 范围内（x11rb 保证）。
            X11Formats::from_screen(setup, &setup.roots[screen_num])
        };
        let (root, root_width, root_height) = {
            let screen = &conn.setup().roots[screen_num];
            (screen.root, screen.width_in_pixels, screen.height_in_pixels)
        };

        Ok(Self {
            conn,
            screen_num,
            root,
            root_width,
            root_height,
            monitor_index,
            monitor,
            formats,
            started: Instant::now(),
            frames_acquired: 0,
        })
    }

    /// 立即抓取一帧。
    ///
    /// X11 的 `GetImage` 没有事件/超时语义，因此 `timeout_ms` 会被忽略：每次
    /// 调用同步取一帧并返回 `Ok(Some(frame))`（请求本身失败时返回 `Err`），
    /// 上层需自行按目标帧率节流。X11 也没有增量脏区信息，故 `dirty` 为整个
    /// 采集区、`full_frame` 恒为 `true`。
    pub fn capture(&mut self, _timeout_ms: u32) -> anyhow::Result<Option<CapturedFrame>> {
        let (x, y, w, h) = self.capture_area();
        let reply = self
            .conn
            .get_image(ImageFormat::Z_PIXMAP, self.root, x, y, w, h, u32::MAX)
            .map_err(|e| anyhow!("X11 GetImage 请求发送失败：{e}"))?
            .reply()
            .map_err(|e| anyhow!("X11 GetImage 失败（{w}x{h}+{x},{y}）：{e}"))?;

        // 协议里 GetImage 回复的 depth/visual 描述「被取图像的那个可绘制对象」。
        // 正常服务器对根窗口总是正确填写；少数服务器/合成器会返回 0/0，
        // 这时退回 setup 中记录的根窗口深度与根视觉，避免整帧直接失败。
        let depth = if reply.depth == 0 {
            self.formats.root_depth
        } else {
            reply.depth
        };
        let visual = if reply.visual == 0 {
            self.formats.root_visual
        } else {
            reply.visual
        };

        let layout = self
            .formats
            .layout(depth, visual)
            .ok_or_else(|| anyhow!("X11: 深度 {depth} 没有可用的像素布局"))?;
        let stride = pick_stride(reply.data.len(), u32::from(w), u32::from(h), &layout)
            .ok_or_else(|| {
                anyhow!(
                    "X11 GetImage 返回 {} 字节，与 {w}x{h}（{} bpp）的行跨距不匹配",
                    reply.data.len(),
                    layout.bits_per_pixel
                )
            })?;
        let data = convert_to_bgra8(&reply.data, u32::from(w), u32::from(h), stride, &layout)
            .map_err(|e| anyhow!("X11 BGRA 转换失败：{e}"))?;

        self.frames_acquired = self.frames_acquired.saturating_add(1);
        // Windows 后端填的是 QueryPerformanceCounter；X11 这边没有等价物，
        // 填「自采集器创建以来的 CLOCK_MONOTONIC 微秒数」——同样是单调递增、
        // 只用于诊断/节流，不会参与编码时间轴（那由 dfy-session 的 pts 负责）。
        let timestamp_qpc = self.started.elapsed().as_micros().min(i64::MAX as u128) as i64;

        Ok(Some(CapturedFrame {
            width: u32::from(w),
            height: u32::from(h),
            stride: u32::from(w) * PixelFormat::Bgra8.bytes_per_pixel(),
            format: PixelFormat::Bgra8,
            data,
            timestamp_qpc,
            dirty: vec![Rect::new(0, 0, i32::from(w), i32::from(h))],
            full_frame: true,
        }))
    }

    /// 当前采集的显示器信息。
    pub fn monitor(&self) -> &MonitorInfo {
        &self.monitor
    }

    /// 成功采集的帧数（诊断/统计用）。
    pub fn frames_acquired(&self) -> u64 {
        self.frames_acquired
    }

    /// 重新枚举显示器并刷新根窗口尺寸（热插拔 / 改分辨率 / 改旋转后调用）。
    ///
    /// 保持 `monitor_index` 不变；若该索引已不存在则返回
    /// [`CaptureError::NoSuchMonitor`]，调用方应重新 `new`。
    pub fn recreate(&mut self) -> anyhow::Result<()> {
        let geoms = {
            let screen = &self.conn.setup().roots[self.screen_num];
            enumerate_geometries(&self.conn, screen.root, screen)?
        };
        let monitor = geometries_to_monitors(&geoms)
            .into_iter()
            .nth(self.monitor_index as usize)
            .ok_or(CaptureError::NoSuchMonitor(self.monitor_index))?;
        let formats = {
            let setup = self.conn.setup();
            X11Formats::from_screen(setup, &setup.roots[self.screen_num])
        };
        let (root, root_width, root_height) = {
            let screen = &self.conn.setup().roots[self.screen_num];
            (screen.root, screen.width_in_pixels, screen.height_in_pixels)
        };
        self.root = root;
        self.root_width = root_width;
        self.root_height = root_height;
        self.monitor = monitor;
        self.formats = formats;
        Ok(())
    }

    /// X11 后端没有需要显式释放的 duplication 对象；本方法仅为与 Windows
    /// 后端保持 API 对称，drop 采集器即关闭连接。
    pub fn release(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    /// 把当前显示器矩形裁剪到根窗口范围内，返回 `GetImage` 需要的
    /// `(x, y, width, height)`。
    ///
    /// 坐标在根窗口里必须非负（负坐标会触发 `BadMatch`/`BadValue`），
    /// 尺寸也必须至少 1 像素，因此这里做饱和裁剪；显示器整体跑出根窗口时
    /// 退化为左上角 1x1（属于异常拓扑，不保证画面正确）。
    fn capture_area(&self) -> (i16, i16, u16, u16) {
        let root_w = u32::from(self.root_width.max(1));
        let root_h = u32::from(self.root_height.max(1));
        let x = self.monitor.x.max(0).min(root_w as i32 - 1) as u32;
        let y = self.monitor.y.max(0).min(root_h as i32 - 1) as u32;
        let w = self
            .monitor
            .w
            .max(1)
            .min(root_w - x)
            .min(u32::from(u16::MAX)) as u16;
        let h = self
            .monitor
            .h
            .max(1)
            .min(root_h - y)
            .min(u32::from(u16::MAX)) as u16;
        (x as i16, y as i16, w, h)
    }
}

impl std::fmt::Debug for LinuxCapturer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 不打印 RustConnection（内部状态冗长且无诊断价值）。
        f.debug_struct("LinuxCapturer")
            .field("screen_num", &self.screen_num)
            .field("root", &self.root)
            .field("root_size", &(self.root_width, self.root_height))
            .field("monitor", &self.monitor)
            .field("frames_acquired", &self.frames_acquired)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// 连接
// ---------------------------------------------------------------------------

/// 连接 `$DISPLAY` 指向的 X 服务器。
///
/// 纯 Wayland 会话（无 XWayland）没有 `DISPLAY`，这里返回
/// [`CaptureError::Unsupported`]，方便上层决定是否切到 Wayland 后端。
fn connect_display() -> anyhow::Result<(RustConnection, usize)> {
    if std::env::var_os("DISPLAY").map_or(true, |value| value.is_empty()) {
        return Err(CaptureError::Unsupported(
            "X11 不可用：环境变量 DISPLAY 未设置（Wayland 会话可能没有 XWayland）",
        )
        .into());
    }
    x11rb::connect(None).map_err(|e| anyhow!("连接 X11 服务器失败（DISPLAY）：{e}"))
}

// ---------------------------------------------------------------------------
// 显示器枚举
// ---------------------------------------------------------------------------

/// 与枚举来源解耦的显示器几何信息。
///
/// 枚举链路（RandR 1.5 / RandR 1.2 / Xinerama / 根窗口）各自产出
/// `MonitorGeometry`，再统一交给 [`geometries_to_monitors`] 编号、判定
/// primary、估算 DPI。这样编号/兜底逻辑可以用纯数据单测覆盖。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MonitorGeometry {
    /// 显示器名（RandR 的 monitor/output 名、`Xinerama-N` 或 `default`）。
    pub name: String,
    /// 在虚拟桌面（根窗口）中的左上角 x。
    pub x: i32,
    /// 在虚拟桌面（根窗口）中的左上角 y。
    pub y: i32,
    /// 宽度（像素）。
    pub w: u32,
    /// 高度（像素）。
    pub h: u32,
    /// 物理宽度（毫米，0 = 未知）。
    pub width_mm: u32,
    /// 物理高度（毫米，0 = 未知）。
    pub height_mm: u32,
    /// RandR 报告的 primary 标志（Xinerama/根窗口兜底时为 false）。
    pub primary: bool,
}

/// 逐级降级枚举显示器几何，永不返回空（兜底是根窗口单屏）。
fn enumerate_geometries(
    conn: &RustConnection,
    root: Window,
    screen: &Screen,
) -> anyhow::Result<Vec<MonitorGeometry>> {
    // 1) RandR：1.5 的 GetMonitors 语义最接近「逻辑显示器」，
    //    旧服务器退到 1.2 的 CRTC/Output 拼装。
    if let Ok(geoms) = randr_geometries(conn, root) {
        if !geoms.is_empty() {
            return Ok(geoms);
        }
    }
    // 2) Xinerama：Xorg 的兼容层，没有 primary/毫米信息。
    if let Ok(geoms) = xinerama_geometries(conn) {
        if !geoms.is_empty() {
            return Ok(geoms);
        }
    }
    // 3) 兜底：整个根窗口当作一个显示器（老服务器 / 单屏 / 无扩展）。
    Ok(vec![root_geometry(screen)])
}

/// RandR 枚举（1.5 GetMonitors 优先，其次 1.2 CRTC/Output）。
///
/// 返回 `Ok(vec![])` 表示「扩展不存在/版本太低」，由调用方继续降级；
/// 返回 `Err` 表示扩展在但请求失败，同样由调用方忽略并降级。
fn randr_geometries(conn: &RustConnection, root: Window) -> anyhow::Result<Vec<MonitorGeometry>> {
    let info = conn
        .extension_information(randr::X11_EXTENSION_NAME)
        .map_err(|e| anyhow!("查询 RANDR 扩展失败：{e}"))?;
    if info.is_none() {
        return Ok(Vec::new());
    }

    let version = conn
        .randr_query_version(1, 6)
        .map_err(|e| anyhow!("RANDR QueryVersion 发送失败：{e}"))?
        .reply()
        .map_err(|e| anyhow!("RANDR QueryVersion 失败：{e}"))?;
    // RandR 1.2 才引入 CRTC/Output，低于 1.2 直接交给 Xinerama/根窗口。
    if version.major_version < 1 || (version.major_version == 1 && version.minor_version < 2) {
        return Ok(Vec::new());
    }

    let mut geoms = if version.major_version > 1 || version.minor_version >= 5 {
        // 1.5 GetMonitors 失败（例如 XWayland 的 RandR 实现不全）时退到 1.2。
        randr15_geometries(conn, root).unwrap_or_default()
    } else {
        Vec::new()
    };
    if geoms.is_empty() {
        geoms = randr12_geometries(conn, root, version.minor_version).unwrap_or_default();
    }
    Ok(geoms)
}

/// RandR ≥ 1.5：一次 `GetMonitors` 拿到合并后的逻辑显示器。
fn randr15_geometries(conn: &RustConnection, root: Window) -> anyhow::Result<Vec<MonitorGeometry>> {
    let reply = conn
        .randr_get_monitors(root, true)
        .map_err(|e| anyhow!("RANDR GetMonitors 发送失败：{e}"))?
        .reply()
        .map_err(|e| anyhow!("RANDR GetMonitors 失败：{e}"))?;

    let mut out = Vec::with_capacity(reply.monitors.len());
    for (index, monitor) in reply.monitors.iter().enumerate() {
        // name 是 ATOM，取不到名字时用序号兜底（不影响采集）。
        let name = atom_name(conn, monitor.name).unwrap_or_else(|| format!("monitor-{index}"));
        if let Some(geometry) = randr_monitor_geometry(monitor, name) {
            out.push(geometry);
        }
    }
    Ok(out)
}

/// `randr::MonitorInfo` → [`MonitorGeometry`]（0 尺寸视为未启用，返回 `None`）。
pub(crate) fn randr_monitor_geometry(
    monitor: &randr::MonitorInfo,
    name: String,
) -> Option<MonitorGeometry> {
    if monitor.width == 0 || monitor.height == 0 {
        return None;
    }
    Some(MonitorGeometry {
        name,
        x: monitor.x as i32,
        y: monitor.y as i32,
        w: u32::from(monitor.width),
        h: u32::from(monitor.height),
        width_mm: monitor.width_in_millimeters,
        height_mm: monitor.height_in_millimeters,
        primary: monitor.primary,
    })
}

/// RandR ≥ 1.2 且 < 1.5：遍历「激活的 CRTC」，用其第一个 output 命名。
fn randr12_geometries(
    conn: &RustConnection,
    root: Window,
    minor_version: u32,
) -> anyhow::Result<Vec<MonitorGeometry>> {
    // GetScreenResourcesCurrent 是 RandR 1.3 才加的；1.2 服务器只有
    // GetScreenResources（会把每个可能的模式都枚举出来，略慢但兼容）。
    let (crtcs, config_timestamp) = if minor_version >= 3 {
        let reply = conn
            .randr_get_screen_resources_current(root)
            .map_err(|e| anyhow!("RANDR GetScreenResourcesCurrent 发送失败：{e}"))?
            .reply()
            .map_err(|e| anyhow!("RANDR GetScreenResourcesCurrent 失败：{e}"))?;
        (reply.crtcs, reply.config_timestamp)
    } else {
        let reply = conn
            .randr_get_screen_resources(root)
            .map_err(|e| anyhow!("RANDR GetScreenResources 发送失败：{e}"))?
            .reply()
            .map_err(|e| anyhow!("RANDR GetScreenResources 失败：{e}"))?;
        (reply.crtcs, reply.config_timestamp)
    };

    // primary output 是 RandR 1.3 才有的请求；1.2 服务器会报错，忽略即可。
    let primary_output = conn
        .randr_get_output_primary(root)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|reply| reply.output);

    let mut out = Vec::new();
    for crtc in &crtcs {
        // CRTC 可能在枚举与查询之间被关掉，单个失败只跳过它。
        let info = match conn
            .randr_get_crtc_info(*crtc, config_timestamp)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
        {
            Some(info) => info,
            None => continue,
        };
        // 未启用的 CRTC：尺寸为 0 或没有绑定 output。
        if info.width == 0 || info.height == 0 || info.outputs.is_empty() {
            continue;
        }

        let mut name: Option<String> = None;
        let mut width_mm = 0u32;
        let mut height_mm = 0u32;
        for output in &info.outputs {
            let Ok(output_info) = conn
                .randr_get_output_info(*output, config_timestamp)
                .map_err(|e| anyhow!("RANDR GetOutputInfo 发送失败：{e}"))
                .and_then(|cookie| {
                    cookie
                        .reply()
                        .map_err(|e| anyhow!("RANDR GetOutputInfo 失败：{e}"))
                })
            else {
                continue;
            };
            if name.is_none() {
                let text = String::from_utf8_lossy(&output_info.name).to_string();
                if !text.is_empty() {
                    name = Some(text);
                }
            }
            if width_mm == 0 {
                width_mm = output_info.mm_width;
                height_mm = output_info.mm_height;
            }
        }

        let name = name.unwrap_or_else(|| format!("CRTC-{crtc}"));
        let primary = primary_output
            .map(|output| output != 0 && info.outputs.contains(&output))
            .unwrap_or(false);
        if let Some(geometry) = crtc_monitor_geometry(
            info.x,
            info.y,
            info.width,
            info.height,
            width_mm,
            height_mm,
            name,
            primary,
        ) {
            out.push(geometry);
        }
    }
    Ok(out)
}

/// CRTC 几何 → [`MonitorGeometry`]（0 尺寸视为未启用，返回 `None`）。
///
/// 旋转过的 CRTC 由服务器直接给出旋转后的宽高，这里无需再换算。
pub(crate) fn crtc_monitor_geometry(
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    width_mm: u32,
    height_mm: u32,
    name: String,
    primary: bool,
) -> Option<MonitorGeometry> {
    if width == 0 || height == 0 {
        return None;
    }
    Some(MonitorGeometry {
        name,
        x: x as i32,
        y: y as i32,
        w: u32::from(width),
        h: u32::from(height),
        width_mm,
        height_mm,
        primary,
    })
}

/// Xinerama 兜底：只有几何信息，没有名字/毫米/primary。
fn xinerama_geometries(conn: &RustConnection) -> anyhow::Result<Vec<MonitorGeometry>> {
    let info = conn
        .extension_information(xinerama::X11_EXTENSION_NAME)
        .map_err(|e| anyhow!("查询 XINERAMA 扩展失败：{e}"))?;
    if info.is_none() {
        return Ok(Vec::new());
    }
    conn.xinerama_query_version(1, 1)
        .map_err(|e| anyhow!("XINERAMA QueryVersion 发送失败：{e}"))?
        .reply()
        .map_err(|e| anyhow!("XINERAMA QueryVersion 失败：{e}"))?;
    let reply = conn
        .xinerama_query_screens()
        .map_err(|e| anyhow!("XINERAMA QueryScreens 发送失败：{e}"))?
        .reply()
        .map_err(|e| anyhow!("XINERAMA QueryScreens 失败：{e}"))?;

    Ok(reply
        .screen_info
        .iter()
        .enumerate()
        .map(|(index, screen)| MonitorGeometry {
            name: format!("Xinerama-{index}"),
            x: screen.x_org as i32,
            y: screen.y_org as i32,
            w: u32::from(screen.width),
            h: u32::from(screen.height),
            width_mm: 0,
            height_mm: 0,
            primary: false,
        })
        .collect())
}

/// 最后兜底：整个根窗口当一块屏。
fn root_geometry(screen: &Screen) -> MonitorGeometry {
    MonitorGeometry {
        name: "default".to_string(),
        x: 0,
        y: 0,
        w: u32::from(screen.width_in_pixels),
        h: u32::from(screen.height_in_pixels),
        width_mm: u32::from(screen.width_in_millimeters),
        height_mm: u32::from(screen.height_in_millimeters),
        primary: true,
    }
}

/// 取 ATOM 对应的名字；失败或空名返回 `None`。
fn atom_name(conn: &RustConnection, atom: Atom) -> Option<String> {
    if atom == 0 {
        return None;
    }
    let reply = conn.get_atom_name(atom).ok()?.reply().ok()?;
    let name = String::from_utf8_lossy(&reply.name).to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

// ---------------------------------------------------------------------------
// 几何 → MonitorInfo（纯逻辑，可单测）
// ---------------------------------------------------------------------------

/// 把几何信息列表编号成 [`MonitorInfo`]。
///
/// 规则：
/// * `id` = 在该列表中的下标（与 Windows 后端「索引即 id」的约定一致）；
/// * primary 只有一个：优先 RandR 的 primary 标志，其次「左上角在 (0,0) 的屏」，
///   再次第一个屏；其余一律置 false；
/// * `dpi` 用物理宽度估算（毫米缺失时退到物理高度，再缺省 96）。
pub(crate) fn geometries_to_monitors(geoms: &[MonitorGeometry]) -> Vec<MonitorInfo> {
    let primary_index = geoms
        .iter()
        .position(|geometry| geometry.primary)
        .or_else(|| {
            geoms
                .iter()
                .position(|geometry| geometry.x == 0 && geometry.y == 0)
        })
        .unwrap_or(0);

    geoms
        .iter()
        .enumerate()
        .map(|(index, geometry)| MonitorInfo {
            id: index as u32,
            name: geometry.name.clone(),
            x: geometry.x,
            y: geometry.y,
            w: geometry.w,
            h: geometry.h,
            primary: index == primary_index,
            dpi: monitor_dpi(geometry),
        })
        .collect()
}

/// 由物理尺寸估算 DPI；宽度优先，其次高度，信息缺失时按 96 处理。
pub(crate) fn monitor_dpi(geometry: &MonitorGeometry) -> u32 {
    if geometry.width_mm > 0 {
        dpi_from_mm(geometry.w, geometry.width_mm)
    } else if geometry.height_mm > 0 {
        dpi_from_mm(geometry.h, geometry.height_mm)
    } else {
        96
    }
}

/// `像素 * 25.4 / 毫米` 得到 DPI（四舍五入，夹在 1..=1000）。
pub(crate) fn dpi_from_mm(pixels: u32, millimeters: u32) -> u32 {
    if pixels == 0 || millimeters == 0 {
        return 96;
    }
    let dpi = f64::from(pixels) * 25.4 / f64::from(millimeters);
    dpi.round().clamp(1.0, 1000.0) as u32
}

// ---------------------------------------------------------------------------
// 像素格式
// ---------------------------------------------------------------------------

/// 一个 visual 的通道掩码（决定 R/G/B 分别占哪些位）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VisualMasks {
    /// visual id。
    pub visual_id: u32,
    /// 该 visual 所属深度。
    pub depth: u8,
    /// 红通道掩码。
    pub red: u32,
    /// 绿通道掩码。
    pub green: u32,
    /// 蓝通道掩码。
    pub blue: u32,
}

/// `Setup.pixmap_formats` 的一项：某深度的每像素位数与扫描线对齐。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PixmapFormat {
    /// 深度。
    pub depth: u8,
    /// 每像素位数（24 位深度常见 32）。
    pub bits_per_pixel: u8,
    /// 扫描线按多少位对齐（通常 32）。
    pub scanline_pad: u8,
}

/// 解码一次 `GetImage` 结果需要的全部像素布局信息。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PixelLayout {
    /// 每像素位数。
    pub bits_per_pixel: u8,
    /// 扫描线对齐位数。
    pub scanline_pad: u8,
    /// 服务器字节序是否为 MSBFirst（X11 默认 LSBFirst）。
    pub msb_first: bool,
    /// 红通道掩码。
    pub red_mask: u32,
    /// 绿通道掩码。
    pub green_mask: u32,
    /// 蓝通道掩码。
    pub blue_mask: u32,
}

impl PixelLayout {
    /// 每像素占用的整字节数（向上取整）。
    pub(crate) fn bytes_per_pixel(&self) -> usize {
        (usize::from(self.bits_per_pixel) + 7) / 8
    }

    /// 一行最少需要多少字节（不考虑扫描线对齐）。
    pub(crate) fn min_row_bytes(&self, width: u32) -> Option<usize> {
        let bits = usize::try_from(width)
            .ok()?
            .checked_mul(usize::from(self.bits_per_pixel))?;
        Some((bits + 7) / 8)
    }

    /// 一行按服务器 `scanline_pad` 对齐后的字节数（ZPixmap 的实际行跨距）。
    pub(crate) fn row_bytes(&self, width: u32) -> Option<usize> {
        let pad = usize::from(self.scanline_pad.max(8));
        let bits = usize::try_from(width)
            .ok()?
            .checked_mul(usize::from(self.bits_per_pixel))?;
        Some(bits.div_ceil(pad).checked_mul(pad)? / 8)
    }
}

/// X11 会话的像素格式表：由 `Setup` + 目标 `Screen` 的视觉信息构建。
#[derive(Debug, Clone)]
pub(crate) struct X11Formats {
    /// 服务器像素字节序是否为 MSBFirst。
    pub msb_first: bool,
    /// 根窗口深度（`GetImage` 最常见的回复深度）。
    pub root_depth: u8,
    /// 根窗口 visual id。
    pub root_visual: u32,
    /// 所有可用 visual 的掩码。
    pub visuals: Vec<VisualMasks>,
    /// 所有深度的 pixmap 格式。
    pub formats: Vec<PixmapFormat>,
}

impl X11Formats {
    /// 从连接 `Setup` 与 `Screen` 收集像素格式信息。
    pub(crate) fn from_screen(setup: &Setup, screen: &Screen) -> Self {
        let mut visuals = Vec::new();
        for depth in &screen.allowed_depths {
            for visual in &depth.visuals {
                visuals.push(VisualMasks {
                    visual_id: visual.visual_id,
                    depth: depth.depth,
                    red: visual.red_mask,
                    green: visual.green_mask,
                    blue: visual.blue_mask,
                });
            }
        }
        let formats = setup
            .pixmap_formats
            .iter()
            .map(|format| PixmapFormat {
                depth: format.depth,
                bits_per_pixel: format.bits_per_pixel,
                scanline_pad: format.scanline_pad,
            })
            .collect();
        Self::from_parts(
            u8::from(setup.image_byte_order) == 1,
            screen.root_depth,
            screen.root_visual,
            formats,
            visuals,
        )
    }

    /// 便于单测的构造函数（不依赖真实 X server）。
    pub(crate) fn from_parts(
        msb_first: bool,
        root_depth: u8,
        root_visual: u32,
        formats: Vec<PixmapFormat>,
        visuals: Vec<VisualMasks>,
    ) -> Self {
        Self {
            msb_first,
            root_depth,
            root_visual,
            visuals,
            formats,
        }
    }

    /// 给出某个 `GetImage` 回复（深度 + visual）对应的像素布局。
    ///
    /// 掩码按「同 visual id」→「根 visual」→「同深度任一 visual」的顺序匹配；
    /// 每像素位数/扫描线对齐取该深度在 `pixmap_formats` 里登记的格式，
    /// 缺失时按 32 位、32 位对齐兜底（Xorg 对 24/32 深度都是这个布局）。
    pub(crate) fn layout(&self, depth: u8, visual: u32) -> Option<PixelLayout> {
        let masks = self
            .visuals
            .iter()
            .find(|candidate| candidate.visual_id == visual && candidate.depth == depth)
            .or_else(|| {
                self.visuals.iter().find(|candidate| {
                    candidate.visual_id == self.root_visual && candidate.depth == depth
                })
            })
            .or_else(|| {
                self.visuals
                    .iter()
                    .find(|candidate| candidate.depth == depth)
            })?;

        if masks.red == 0 && masks.green == 0 && masks.blue == 0 {
            // PseudoColor/StaticGray 之类没有 RGB 掩码的视觉，无法直接转 BGRA8。
            return None;
        }

        let format = self
            .formats
            .iter()
            .find(|format| format.depth == depth)
            .copied()
            .unwrap_or(PixmapFormat {
                depth,
                bits_per_pixel: 32,
                scanline_pad: 32,
            });
        if format.bits_per_pixel == 0 || format.bits_per_pixel > MAX_BITS_PER_PIXEL {
            return None;
        }

        Some(PixelLayout {
            bits_per_pixel: format.bits_per_pixel,
            scanline_pad: format.scanline_pad,
            msb_first: self.msb_first,
            red_mask: masks.red,
            green_mask: masks.green,
            blue_mask: masks.blue,
        })
    }
}

/// 从 `GetImage` 回复的实际字节数推断行跨距。
///
/// 优先用按 `scanline_pad` 对齐的理论跨距；若服务器多给了尾部/行内 padding，
/// 则接受「字节数能被行数整除且不小于一行最小字节数」的跨距。两者都不成立
/// 说明布局假设不成立，返回 `None` 让上层报错而不是产出错图。
pub(crate) fn pick_stride(
    data_len: usize,
    width: u32,
    height: u32,
    layout: &PixelLayout,
) -> Option<usize> {
    if width == 0 || height == 0 {
        return None;
    }
    let rows = usize::try_from(height).ok()?;
    let min_row = layout.min_row_bytes(width)?;
    let preferred = layout.row_bytes(width)?;
    if preferred.checked_mul(rows)? == data_len {
        return Some(preferred);
    }
    if data_len % rows == 0 {
        let server_stride = data_len / rows;
        if server_stride >= min_row {
            return Some(server_stride);
        }
    }
    if preferred.checked_mul(rows)? <= data_len {
        return Some(preferred);
    }
    None
}

/// 把一次 `GetImage` 的原始像素转成 tightly-packed BGRA8。
///
/// * LSBFirst（X11 默认）：按小端整数解读像素，用掩码取通道；
/// * MSBFirst：按大端整数解读，逐像素重排（返回的字节序仍是 B,G,R,A）；
/// * 深度 24 且每像素 32 位时高字节由服务器定义不明 → **一律 alpha=255**；
/// * 非 8 位的通道（如 10-bit）按掩码位宽线性缩放到 0..=255。
pub(crate) fn convert_to_bgra8(
    src: &[u8],
    width: u32,
    height: u32,
    stride: usize,
    layout: &PixelLayout,
) -> Result<Vec<u8>, String> {
    let w = usize::try_from(width).map_err(|e| e.to_string())?;
    let h = usize::try_from(height).map_err(|e| e.to_string())?;
    if w == 0 || h == 0 {
        return Err("图像宽高不能为 0".to_string());
    }

    let bpp = layout.bytes_per_pixel();
    if bpp == 0 || bpp > 4 {
        return Err(format!(
            "不支持的每像素位数 {}（仅支持 8/16/24/32）",
            layout.bits_per_pixel
        ));
    }
    let min_row = layout
        .min_row_bytes(width)
        .ok_or_else(|| "行字节数溢出".to_string())?;
    if stride < min_row {
        return Err(format!("行跨距 {stride} 小于一行最少字节数 {min_row}"));
    }
    let needed = stride
        .checked_mul(h)
        .ok_or_else(|| "像素缓冲区长度溢出".to_string())?;
    if src.len() < needed {
        return Err(format!(
            "像素缓冲区过短：{} < {needed}（{w}x{h}, stride {stride}）",
            src.len()
        ));
    }

    let out_row = w
        .checked_mul(4)
        .ok_or_else(|| "输出缓冲区长度溢出".to_string())?;
    let mut out = vec![0u8; out_row * h];

    // 快路径：32bpp、LSBFirst、标准 B8G8R8A8 掩码（Xorg 24/32 深度的常态）。
    // 此时内存里已经是 B,G,R,X，只需把未定义的 X 字节换成不透明 alpha。
    if layout.bits_per_pixel == 32
        && !layout.msb_first
        && layout.red_mask == 0x00ff_0000
        && layout.green_mask == 0x0000_ff00
        && layout.blue_mask == 0x0000_00ff
    {
        for y in 0..h {
            let row = &src[y * stride..y * stride + w * 4];
            let dst = &mut out[y * out_row..(y + 1) * out_row];
            for x in 0..w {
                let pixel = &row[x * 4..x * 4 + 4];
                dst[x * 4] = pixel[0];
                dst[x * 4 + 1] = pixel[1];
                dst[x * 4 + 2] = pixel[2];
                dst[x * 4 + 3] = 255;
            }
        }
        return Ok(out);
    }

    // 通用路径：任意字节序 + 任意（连续）通道掩码。
    for y in 0..h {
        let row = &src[y * stride..];
        for x in 0..w {
            let value = read_pixel(row, x * bpp, bpp, layout.msb_first);
            let offset = y * out_row + x * 4;
            out[offset] = scale_channel(value, layout.blue_mask);
            out[offset + 1] = scale_channel(value, layout.green_mask);
            out[offset + 2] = scale_channel(value, layout.red_mask);
            // X11 没有可靠的 alpha（深度 24/32 的高字节未定义），统一置不透明。
            out[offset + 3] = 255;
        }
    }
    Ok(out)
}

/// 按给定字节序从一行像素里读出一个整数像素值。
fn read_pixel(row: &[u8], offset: usize, bytes: usize, msb_first: bool) -> u32 {
    let mut value = 0u32;
    for index in 0..bytes {
        let byte = u32::from(row[offset + index]);
        if msb_first {
            value = (value << 8) | byte;
        } else {
            value |= byte << (8 * index);
        }
    }
    value
}

/// 用掩码取一个通道并线性缩放到 0..=255（掩码不连续时按连续处理）。
fn scale_channel(value: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    let bits = mask.count_ones().min(32);
    let raw = (value & mask) >> shift;
    if bits >= 32 {
        return (raw >> 24) as u8;
    }
    let max = (1u64 << bits) - 1;
    ((u64::from(raw) * 255 + max / 2) / max) as u8
}

// ---------------------------------------------------------------------------
// 单测：全部是纯数据测试，不需要真实 X server
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    // 显式导入，避免依赖 `use super::*` 对私有 `use` 的可见性规则。
    use x11rb::protocol::randr;

    /// 标准的 32bpp / LSBFirst / B8G8R8A8 布局。
    fn layout_bgra_lsb() -> PixelLayout {
        PixelLayout {
            bits_per_pixel: 32,
            scanline_pad: 32,
            msb_first: false,
            red_mask: 0x00ff_0000,
            green_mask: 0x0000_ff00,
            blue_mask: 0x0000_00ff,
        }
    }

    #[test]
    fn converts_lsb_32bit_and_forces_opaque_alpha() {
        // 两个像素：内存里是 B,G,R,X；X 为未定义的高字节，转换后必须变成 255。
        let src = [
            0x33, 0x22, 0x11, 0xAA, // B=0x33, G=0x22, R=0x11, X=0xAA
            0xFF, 0x00, 0x00, 0x7F, // 纯蓝，X=0x7F
        ];
        let out = convert_to_bgra8(&src, 2, 1, 8, &layout_bgra_lsb()).unwrap();
        assert_eq!(out, vec![0x33, 0x22, 0x11, 0xFF, 0xFF, 0x00, 0x00, 0xFF]);
    }

    #[test]
    fn converts_msb_first_32bit_with_byte_swapping() {
        let mut layout = layout_bgra_lsb();
        layout.msb_first = true;
        // MSBFirst 下内存里是 X,R,G,B（大端 0x00RRGGBB）。
        let src = [0xAA, 0x11, 0x22, 0x33];
        let out = convert_to_bgra8(&src, 1, 1, 4, &layout).unwrap();
        assert_eq!(out, vec![0x33, 0x22, 0x11, 0xFF]);
    }

    #[test]
    fn converts_24bpp_with_scanline_padding() {
        // 2 像素 * 24bpp = 48 bit → 按 32 bit 对齐成 64 bit = 8 字节/行。
        let mut layout = layout_bgra_lsb();
        layout.bits_per_pixel = 24;
        assert_eq!(layout.row_bytes(2), Some(8));
        assert_eq!(layout.min_row_bytes(2), Some(6));
        // 第二行带 padding，且 padding 字节是垃圾值，不能被当成像素。
        let src = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0xEE, 0xEE, // 第 1 行
        ];
        let out = convert_to_bgra8(&src, 2, 1, 8, &layout).unwrap();
        assert_eq!(out, vec![0x01, 0x02, 0x03, 0xFF, 0x04, 0x05, 0x06, 0xFF]);

        assert_eq!(pick_stride(src.len(), 2, 1, &layout), Some(8));
    }

    #[test]
    fn converts_10bit_masks_by_scaling() {
        let layout = PixelLayout {
            bits_per_pixel: 32,
            scanline_pad: 32,
            msb_first: false,
            red_mask: 0x3ff0_0000,
            green_mask: 0x000f_fc00,
            blue_mask: 0x0000_03ff,
        };
        // R=0x3ff（满量程）, G=0, B=0x200（半量程）。
        let value: u32 = (0x3ff << 20) | 0x200;
        let src = value.to_le_bytes();
        let out = convert_to_bgra8(&src, 1, 1, 4, &layout).unwrap();
        assert_eq!(out[3], 0xFF);
        assert_eq!(out[2], 255); // R
        assert_eq!(out[1], 0); // G
                               // 0x200/0x3ff ≈ 0.5005 → 128
        assert_eq!(out[0], 128); // B
    }

    #[test]
    fn rejects_short_or_misaligned_buffers() {
        let layout = layout_bgra_lsb();
        // 需要 4 字节，只给 3 字节。
        assert!(convert_to_bgra8(&[0, 0, 0], 1, 1, 4, &layout).is_err());
        // stride 比一行最小值还小。
        assert!(convert_to_bgra8(&[0; 8], 2, 1, 3, &layout).is_err());
        // 0 尺寸。
        assert!(convert_to_bgra8(&[], 0, 1, 0, &layout).is_err());
        // 行跨距推断：给的数据装不下。
        assert_eq!(pick_stride(6, 2, 1, &layout), None);
    }

    #[test]
    fn pick_stride_accepts_server_side_padding() {
        let layout = layout_bgra_lsb();
        // 理论 8 字节/行，服务器给了 16 字节/行（多出的 padding 合法）。
        assert_eq!(pick_stride(16, 2, 1, &layout), Some(16));
        // 两行，理论 8*2=16 正好相等。
        assert_eq!(pick_stride(16, 2, 2, &layout), Some(8));
    }

    #[test]
    fn row_bytes_respects_scanline_pad() {
        let mut layout = layout_bgra_lsb();
        assert_eq!(layout.row_bytes(3), Some(12));
        layout.bits_per_pixel = 16;
        assert_eq!(layout.min_row_bytes(5), Some(10));
        // 5 像素 * 16bit = 80 bit → 对齐到 96 bit = 12 字节。
        assert_eq!(layout.row_bytes(5), Some(12));
    }

    #[test]
    fn dpi_from_mm_rounds_and_falls_back() {
        // 1920 px / 509 mm ≈ 95.8 dpi → 96
        assert_eq!(dpi_from_mm(1920, 509), 96);
        // 3840 / 600 ≈ 162.56 → 163
        assert_eq!(dpi_from_mm(3840, 600), 163);
        assert_eq!(dpi_from_mm(1920, 0), 96);
        assert_eq!(dpi_from_mm(0, 300), 96);
        // 极端值被夹住。
        assert_eq!(dpi_from_mm(100_000, 1), 1000);
    }

    #[test]
    fn geometries_assign_ids_and_pick_primary_fallback() {
        let geoms = vec![
            MonitorGeometry {
                name: "HDMI-1".into(),
                x: 0,
                y: 0,
                w: 1920,
                h: 1080,
                width_mm: 509,
                height_mm: 286,
                primary: false,
            },
            MonitorGeometry {
                name: "DP-1".into(),
                x: 1920,
                y: 0,
                w: 2560,
                h: 1440,
                width_mm: 0,
                height_mm: 0,
                primary: false,
            },
        ];
        let monitors = geometries_to_monitors(&geoms);
        assert_eq!(monitors.len(), 2);
        assert_eq!(monitors[0].id, 0);
        assert_eq!(monitors[1].id, 1);
        // 没有 primary 标志时，左上角在 (0,0) 的屏胜出。
        assert!(monitors[0].primary);
        assert!(!monitors[1].primary);
        assert_eq!(monitors[0].dpi, 96);
        // 无物理尺寸信息 → 96。
        assert_eq!(monitors[1].dpi, 96);
    }

    #[test]
    fn geometries_respect_explicit_primary_and_clear_duplicates() {
        let geoms = vec![
            MonitorGeometry {
                name: "a".into(),
                x: 1920,
                y: 0,
                w: 1920,
                h: 1080,
                width_mm: 0,
                height_mm: 0,
                primary: true,
            },
            MonitorGeometry {
                name: "b".into(),
                x: 0,
                y: 0,
                w: 1920,
                h: 1080,
                width_mm: 0,
                height_mm: 0,
                primary: false,
            },
        ];
        let monitors = geometries_to_monitors(&geoms);
        assert!(monitors[0].primary);
        assert!(!monitors[1].primary);

        // 即使给了两个 primary 标志，也只保留第一个。
        let mut duplicated = geoms.clone();
        duplicated[1].primary = true;
        let monitors = geometries_to_monitors(&duplicated);
        assert!(monitors[0].primary);
        assert!(!monitors[1].primary);
    }

    #[test]
    fn randr_monitor_geometry_maps_fields_and_skips_disabled() {
        let monitor = randr::MonitorInfo {
            name: 42,
            primary: true,
            automatic: false,
            x: -1920,
            y: 0,
            width: 1920,
            height: 1080,
            width_in_millimeters: 509,
            height_in_millimeters: 286,
            outputs: Vec::new(),
        };
        let geometry = randr_monitor_geometry(&monitor, "DP-1".to_string()).unwrap();
        assert_eq!(geometry.name, "DP-1");
        assert_eq!(geometry.x, -1920);
        assert_eq!(geometry.w, 1920);
        assert_eq!(geometry.width_mm, 509);
        assert!(geometry.primary);

        let disabled = randr::MonitorInfo {
            width: 0,
            height: 0,
            ..monitor
        };
        assert!(randr_monitor_geometry(&disabled, "off".into()).is_none());
    }

    #[test]
    fn crtc_monitor_geometry_skips_disabled_crtc() {
        assert!(crtc_monitor_geometry(0, 0, 0, 1080, 0, 0, "x".into(), false).is_none());
        let geometry =
            crtc_monitor_geometry(1920, 0, 2560, 1440, 600, 340, "DP-2".into(), true).unwrap();
        assert_eq!(geometry.x, 1920);
        assert_eq!(geometry.w, 2560);
        assert_eq!(geometry.height_mm, 340);
        assert!(geometry.primary);
    }

    fn formats_standard() -> X11Formats {
        X11Formats::from_parts(
            false,
            24,
            0x21,
            vec![
                PixmapFormat {
                    depth: 24,
                    bits_per_pixel: 32,
                    scanline_pad: 32,
                },
                PixmapFormat {
                    depth: 32,
                    bits_per_pixel: 32,
                    scanline_pad: 32,
                },
            ],
            vec![
                VisualMasks {
                    visual_id: 0x21,
                    depth: 24,
                    red: 0x00ff_0000,
                    green: 0x0000_ff00,
                    blue: 0x0000_00ff,
                },
                VisualMasks {
                    visual_id: 0x3cf,
                    depth: 32,
                    red: 0x00ff_0000,
                    green: 0x0000_ff00,
                    blue: 0x0000_00ff,
                },
            ],
        )
    }

    #[test]
    fn formats_layout_prefers_exact_visual_then_root_visual() {
        let formats = formats_standard();
        let layout = formats.layout(24, 0x21).unwrap();
        assert_eq!(layout.bits_per_pixel, 32);
        assert_eq!(layout.scanline_pad, 32);
        assert!(!layout.msb_first);
        assert_eq!(layout.red_mask, 0x00ff_0000);

        // 未知 visual id → 退到根 visual 的掩码。
        let fallback = formats.layout(24, 0xdead).unwrap();
        assert_eq!(fallback.red_mask, 0x00ff_0000);

        // 深度 32 的 visual 也能拿到。
        assert!(formats.layout(32, 0x3cf).is_some());
        // 完全不存在的深度 → None。
        assert!(formats.layout(8, 0x21).is_none());
    }

    #[test]
    fn formats_layout_returns_none_for_maskless_visual() {
        let formats = X11Formats::from_parts(
            false,
            8,
            0x30,
            vec![PixmapFormat {
                depth: 8,
                bits_per_pixel: 8,
                scanline_pad: 8,
            }],
            vec![VisualMasks {
                visual_id: 0x30,
                depth: 8,
                red: 0,
                green: 0,
                blue: 0,
            }],
        );
        assert!(formats.layout(8, 0x30).is_none());
    }
}
