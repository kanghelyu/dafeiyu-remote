//! Windows DXGI Desktop Duplication backend for [`DxgiCapturer`].
//!
//! Call order follows the Microsoft Desktop Duplication documentation:
//!
//! 1. `CreateDXGIFactory1` → `IDXGIFactory1::EnumAdapters1` → `IDXGIAdapter::EnumOutputs`.
//! 2. Create a `ID3D11Device` on the *same* adapter as the chosen output.
//! 3. `IDXGIOutput1::DuplicateOutput(device)`.
//! 4. Per frame: `AcquireNextFrame` → copy the desktop texture into a
//!    `D3D11_USAGE_STAGING` texture with `D3D11_CPU_ACCESS_READ` →
//!    read `GetFrameDirtyRects` / `GetFrameMoveRects` while the frame is still
//!    owned → `ReleaseFrame` → `Map` the staging texture and copy the rows.
//!
//! This is a from-scratch implementation written against the public API
//! documentation; no third-party capture code was copied.

use std::marker::PhantomData;
use std::mem::size_of;
use std::sync::Once;

use anyhow::{anyhow, Context, Result};
use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, RECT, RPC_E_CHANGED_MODE};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1,
    D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, IDXGIOutput1,
    IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_MORE_DATA,
    DXGI_ERROR_NOT_FOUND, DXGI_ERROR_SESSION_DISCONNECTED, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_DESC, DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_MOVE_RECT, DXGI_OUTPUT_DESC,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::Performance::QueryPerformanceCounter;
use windows::Win32::UI::HiDpi::{
    GetDpiForMonitor, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    MDT_EFFECTIVE_DPI,
};

use crate::{CaptureError, CapturedFrame, MonitorInfo, PixelFormat, Rect};

/// Maximum number of duplication rebuilds attempted for one [`DxgiCapturer::capture`]
/// call after `DXGI_ERROR_ACCESS_LOST`.
const MAX_REBUILD_ATTEMPTS: u32 = 3;

/// One attached DXGI output together with the adapter that drives it.
struct DesktopOutput {
    adapter: IDXGIAdapter1,
    output: IDXGIOutput,
    desc: DXGI_OUTPUT_DESC,
}

impl DesktopOutput {
    fn monitor_info(&self, id: u32, primary: bool, dpi: u32) -> MonitorInfo {
        let name = utf16_array_to_string(&self.desc.DeviceName);
        MonitorInfo {
            id,
            name,
            x: self.desc.DesktopCoordinates.left,
            y: self.desc.DesktopCoordinates.top,
            w: (self.desc.DesktopCoordinates.right - self.desc.DesktopCoordinates.left).max(0)
                as u32,
            h: (self.desc.DesktopCoordinates.bottom - self.desc.DesktopCoordinates.top).max(0)
                as u32,
            primary,
            dpi,
        }
    }
}

fn utf16_array_to_string(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// Calls `CoInitializeEx(MTA)` for the current thread if COM is not initialised
/// yet. `RPC_E_CHANGED_MODE` means the thread already lives in a different
/// apartment, which is fine for DXGI/D3D11 — we simply keep that apartment.
fn ensure_com_initialized() -> Result<()> {
    // SAFETY: standard COM initialisation; no pointers are retained.
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if hr.is_ok() || hr == RPC_E_CHANGED_MODE {
        Ok(())
    } else {
        Err(anyhow!("CoInitializeEx(MTA) failed: 0x{:08X}", hr.0))
    }
}

/// Makes the process per-monitor DPI aware (best effort, once per process).
///
/// Without this, DXGI reports `DXGI_OUTPUT_DESC::DesktopCoordinates` in
/// *virtualized* (logical) pixels for DPI-unaware processes — e.g. 1400x920 for
/// a 2800x1840 output at 200% scaling — while the duplicated texture is always
/// in physical pixels. Awareness keeps both in the same (physical) space.
///
/// This is a process-wide setting; the call fails harmlessly when the host
/// application already picked an awareness mode (manifest or API), in which
/// case we simply keep whatever the host chose.
fn ensure_dpi_awareness() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: plain Win32 call without pointers.
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
    });
}

/// Enumerates all desktop-attached outputs in a stable order (adapter 0's
/// outputs first, then adapter 1's, ...). This order defines the monitor ids
/// used by [`DxgiCapturer::new`] and [`DxgiCapturer::monitors`].
fn enumerate_outputs() -> Result<Vec<DesktopOutput>> {
    ensure_dpi_awareness();
    // SAFETY: CreateDXGIFactory1 and the Enum* calls only touch DXGI objects we
    // own; the returned interface pointers stay alive in `outputs`.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().context("CreateDXGIFactory1 failed")?;
        let mut outputs = Vec::new();

        for adapter_index in 0u32.. {
            let adapter = match factory.EnumAdapters1(adapter_index) {
                Ok(adapter) => adapter,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(e) => {
                    return Err(anyhow!(e)).context(format!(
                        "IDXGIFactory1::EnumAdapters1({adapter_index}) failed"
                    ))
                }
            };

            for output_index in 0u32.. {
                let output = match adapter.EnumOutputs(output_index) {
                    Ok(output) => output,
                    Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                    // An adapter without outputs (headless dGPU) is not fatal:
                    // just move on to the next adapter.
                    Err(_) => break,
                };
                let desc: DXGI_OUTPUT_DESC =
                    output.GetDesc().context("IDXGIOutput::GetDesc failed")?;
                if !desc.AttachedToDesktop.as_bool() {
                    continue;
                }
                outputs.push(DesktopOutput {
                    adapter: adapter.clone(),
                    output,
                    desc,
                });
            }
        }

        Ok(outputs)
    }
}

/// Queries the effective DPI of an output; 0 when unavailable.
fn effective_dpi(monitor: windows::Win32::Graphics::Gdi::HMONITOR) -> u32 {
    let mut dpi_x = 0u32;
    let mut dpi_y = 0u32;
    // SAFETY: HMONITOR comes from DXGI_OUTPUT_DESC and is valid while the
    // output is alive.
    let ok = unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
    if ok.is_ok() {
        dpi_x
    } else {
        0
    }
}

fn rect_from_win(r: &RECT) -> Rect {
    Rect {
        left: r.left,
        top: r.top,
        right: r.right,
        bottom: r.bottom,
    }
}

/// QueryPerformanceCounter value (ticks).
fn qpc_ticks() -> i64 {
    let mut ticks = 0i64;
    // SAFETY: standard QPC call with a valid out pointer.
    unsafe {
        let _ = QueryPerformanceCounter(&mut ticks);
    }
    ticks
}

/// A DXGI Desktop Duplication capturer.
///
/// **Not `Send`**: the COM / D3D11 objects it owns are bound to the thread that
/// created them (and to the desktop that was active at that moment). Create one
/// instance per capturing thread, and use/drop it on that thread only. DXGI also
/// allows only one active duplication object per output, so do not create two
/// capturers for the same monitor.
pub struct DxgiCapturer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    /// Kept so the duplication can be rebuilt after `DXGI_ERROR_ACCESS_LOST`.
    output: IDXGIOutput,
    duplication: Option<IDXGIOutputDuplication>,
    staging: Option<ID3D11Texture2D>,
    staging_desc: D3D11_TEXTURE2D_DESC,
    monitor: MonitorInfo,
    rotation: DXGI_MODE_ROTATION,
    frames_acquired: u64,
    rebuilds: u64,
    /// Makes the type `!Send` on purpose; see the type-level docs.
    _not_send: PhantomData<*mut ()>,
}

impl DxgiCapturer {
    /// Creates a capturer for the monitor at `monitor_index` (0-based, in the
    /// order returned by [`DxgiCapturer::monitors`]).
    ///
    /// COM is initialised as MTA on the calling thread if needed. Call this, and
    /// every other method, from the same thread.
    pub fn new(monitor_index: u32) -> Result<Self> {
        ensure_com_initialized()?;

        let outputs = enumerate_outputs()?;
        if outputs.is_empty() {
            return Err(anyhow!("no desktop-attached DXGI output found"));
        }
        let selected = outputs
            .into_iter()
            .nth(monitor_index as usize)
            .ok_or_else(|| anyhow!(CaptureError::NoSuchMonitor(monitor_index)))?;

        // SAFETY: the adapter/output interfaces are alive in `selected`; the
        // D3D11 device is created on the same adapter as the output, as
        // required by DuplicateOutput.
        unsafe {
            let mut device: Option<ID3D11Device> = None;
            let mut feature_level = D3D_FEATURE_LEVEL::default();
            let mut context: Option<ID3D11DeviceContext> = None;
            let feature_levels = [
                D3D_FEATURE_LEVEL_11_0,
                D3D_FEATURE_LEVEL_10_1,
                D3D_FEATURE_LEVEL_10_0,
            ];
            D3D11CreateDevice(
                &selected.adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE(std::ptr::null_mut()),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&feature_levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut feature_level),
                Some(&mut context),
            )
            .context("D3D11CreateDevice failed")?;
            let device = device.context("D3D11CreateDevice returned no device")?;
            let context = context.context("D3D11CreateDevice returned no context")?;

            let monitor = selected.monitor_info(
                monitor_index,
                is_primary(&selected.desc),
                effective_dpi(selected.desc.Monitor),
            );
            let output = selected.output;
            let output1: IDXGIOutput1 = output
                .cast()
                .context("IDXGIOutput -> IDXGIOutput1 cast failed (needs DXGI 1.2)")?;
            let duplication: IDXGIOutputDuplication = output1
                .DuplicateOutput(&device)
                .context("IDXGIOutput1::DuplicateOutput failed")?;
            let dup_desc: DXGI_OUTDUPL_DESC = duplication.GetDesc();
            let rotation = dup_desc.Rotation;

            Ok(DxgiCapturer {
                device,
                context,
                output,
                duplication: Some(duplication),
                staging: None,
                staging_desc: D3D11_TEXTURE2D_DESC::default(),
                monitor,
                rotation,
                frames_acquired: 0,
                rebuilds: 0,
                _not_send: PhantomData,
            })
        }
    }

    /// Lists the desktop-attached monitors, in the index order used by
    /// [`DxgiCapturer::new`].
    pub fn monitors() -> Result<Vec<MonitorInfo>> {
        ensure_com_initialized()?;
        let outputs = enumerate_outputs()?;
        let mut monitors = Vec::with_capacity(outputs.len());
        for (index, output) in outputs.iter().enumerate() {
            monitors.push(output.monitor_info(
                index as u32,
                is_primary(&output.desc),
                // SAFETY: the output is alive for the duration of this loop.
                effective_dpi(output.desc.Monitor),
            ));
        }
        // Defensive: if no output sits at the virtual-desktop origin (should not
        // happen on Windows), mark the first one primary.
        if !monitors.iter().any(|m| m.primary) {
            if let Some(first) = monitors.first_mut() {
                first.primary = true;
            }
        }
        Ok(monitors)
    }

    /// Metadata for the monitor this capturer was created for.
    pub fn monitor(&self) -> &MonitorInfo {
        &self.monitor
    }

    /// Number of frames successfully read so far (diagnostics).
    pub fn frames_acquired(&self) -> u64 {
        self.frames_acquired
    }

    /// Number of times the duplication object had to be rebuilt after
    /// `DXGI_ERROR_ACCESS_LOST` (diagnostics).
    pub fn rebuilds(&self) -> u64 {
        self.rebuilds
    }

    /// Captures the next desktop frame, waiting up to `timeout_ms` for the
    /// compositor to present one.
    ///
    /// * `Ok(Some(frame))` — a full CPU copy of the desktop.
    /// * `Ok(None)` — nothing new was presented within `timeout_ms`
    ///   (`DXGI_ERROR_WAIT_TIMEOUT`).
    /// * `Err(_)` — fatal error (or `DXGI_ERROR_ACCESS_LOST` persisting after
    ///   the duplication was rebuilt).
    ///
    /// Note: right after a duplication object is created (and after every
    /// rebuild), DXGI can hand out one all-zero (black) frame before the first
    /// real desktop image is presented — observed with a virtual display
    /// adapter. The frame is still a valid, correctly sized buffer; callers that
    /// stream pixels should drop leading blank frames rather than encode them.
    pub fn capture(&mut self, timeout_ms: u32) -> Result<Option<CapturedFrame>> {
        for attempt in 0..MAX_REBUILD_ATTEMPTS {
            if self.duplication.is_none() {
                self.rebuild_duplication()
                    .context("rebuilding the desktop duplication object failed")?;
            }

            let dup = self
                .duplication
                .as_ref()
                .expect("duplication was just (re)built")
                // Clone the COM pointer so the immutable borrow of `self`
                // ends before we call `&mut self` methods below.
                .clone();

            let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            // SAFETY: `dup` is a live IDXGIOutputDuplication; both out
            // parameters are valid stack locals.
            let acquired =
                unsafe { dup.AcquireNextFrame(timeout_ms, &mut frame_info, &mut resource) };

            match acquired {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
                Err(e)
                    if e.code() == DXGI_ERROR_ACCESS_LOST
                        || e.code() == DXGI_ERROR_SESSION_DISCONNECTED =>
                {
                    // Desktop switched / mode changed / secure desktop: DXGI asks
                    // us to release the duplication interface and create a new one.
                    self.duplication = None;
                    self.rebuilds = self.rebuilds.saturating_add(1);
                    if attempt + 1 == MAX_REBUILD_ATTEMPTS {
                        return Err(anyhow!(e)).context(
                            "AcquireNextFrame kept failing with ACCESS_LOST after rebuild",
                        );
                    }
                    // Retry immediately with a fresh duplication object.
                    continue;
                }
                Err(e) => return Err(anyhow!(e)).context("AcquireNextFrame failed"),
            }

            let timestamp_qpc = qpc_ticks();
            // Anything from here on must release the frame, otherwise DXGI
            // refuses to deliver new ones.
            match self.read_acquired_frame(&dup, &frame_info, resource, timestamp_qpc) {
                Ok(Some(frame)) => {
                    self.frames_acquired = self.frames_acquired.saturating_add(1);
                    return Ok(Some(frame));
                }
                Ok(None) => return Ok(None),
                Err(e) if is_access_lost(&e) => {
                    self.duplication = None;
                    self.rebuilds = self.rebuilds.saturating_add(1);
                    if attempt + 1 == MAX_REBUILD_ATTEMPTS {
                        return Err(e);
                    }
                    continue;
                }
                Err(e) => return Err(e),
            }
        }

        Err(anyhow!(
            "desktop duplication could not deliver a frame after {MAX_REBUILD_ATTEMPTS} rebuild attempts"
        ))
    }

    /// Copy the acquired desktop texture to CPU memory and release the frame.
    fn read_acquired_frame(
        &mut self,
        dup: &IDXGIOutputDuplication,
        frame_info: &DXGI_OUTDUPL_FRAME_INFO,
        resource: Option<IDXGIResource>,
        timestamp_qpc: i64,
    ) -> Result<Option<CapturedFrame>> {
        let resource = match resource {
            Some(resource) => resource,
            None => {
                // Should not happen after a successful AcquireNextFrame.
                // SAFETY: we own an acquired frame here.
                let _ = unsafe { dup.ReleaseFrame() };
                return Ok(None);
            }
        };

        // SAFETY: the desktop resource is only valid while we own the frame;
        // all calls below happen before ReleaseFrame.
        unsafe {
            let texture: ID3D11Texture2D = resource
                .cast()
                .context("IDXGIResource -> ID3D11Texture2D cast failed")?;

            let mut texture_desc = D3D11_TEXTURE2D_DESC::default();
            texture.GetDesc(&mut texture_desc);
            if texture_desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
                // TODO(HDR): formats such as R10G10B10A2 / R16G16B16A16_FLOAT are
                // not converted yet. Report the frame as unsupported instead of
                // handing out mismatched pixels.
                let _ = dup.ReleaseFrame();
                return Err(anyhow!(
                    "unsupported DXGI desktop format {:?} (only B8G8R8A8_UNORM is implemented)",
                    texture_desc.Format
                ));
            }

            self.ensure_staging(&texture_desc)?;
            let staging = self
                .staging
                .as_ref()
                .expect("staging texture was just created");
            self.context.CopyResource(staging, &texture);

            // Metadata must be queried while the frame is still owned.
            let dirty = read_frame_metadata(dup, frame_info);

            // Release before Map so the compositor can keep producing frames
            // while we copy the staging texture.
            let _ = dup.ReleaseFrame();

            let frame = self.read_staging(
                texture_desc.Width,
                texture_desc.Height,
                timestamp_qpc,
                dirty,
            )?;
            Ok(Some(frame))
        }
    }

    /// Maps the staging texture and copies its rows into a `CapturedFrame`.
    fn read_staging(
        &mut self,
        width: u32,
        height: u32,
        timestamp_qpc: i64,
        dirty: Vec<Rect>,
    ) -> Result<CapturedFrame> {
        let staging = self
            .staging
            .as_ref()
            .ok_or_else(|| anyhow!("staging texture missing"))?;

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: the staging texture was created with D3D11_USAGE_STAGING +
        // D3D11_CPU_ACCESS_READ; Map/Unmap are paired below.
        unsafe {
            self.context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped as *mut _))
                .context("ID3D11DeviceContext::Map(staging) failed")?;
        }

        let stride = mapped.RowPitch;
        let row_bytes = stride as usize;
        let height_usize = height as usize;
        let mut data = vec![0u8; row_bytes * height_usize];

        // SAFETY: Map returned a readable buffer of at least RowPitch * Height
        // bytes for this staging texture (D3D11 guarantees that for READ maps).
        unsafe {
            let src = mapped.pData as *const u8;
            for y in 0..height_usize {
                let src_row = src.add(y * row_bytes);
                let dst_row = data.as_mut_ptr().add(y * row_bytes);
                std::ptr::copy_nonoverlapping(src_row, dst_row, row_bytes);
            }
            self.context.Unmap(staging, 0);
        }

        let dirty_empty = dirty.is_empty();
        Ok(CapturedFrame {
            width,
            height,
            stride,
            format: PixelFormat::Bgra8,
            data,
            timestamp_qpc,
            dirty,
            full_frame: dirty_empty,
        })
    }

    /// Creates (or recreates) the staging texture to match the acquired desktop
    /// texture.
    fn ensure_staging(&mut self, source: &D3D11_TEXTURE2D_DESC) -> Result<()> {
        let matches = match &self.staging_desc {
            desc if self.staging.is_some() => {
                desc.Width == source.Width
                    && desc.Height == source.Height
                    && desc.Format == source.Format
            }
            _ => false,
        };
        if matches {
            return Ok(());
        }

        let desc = D3D11_TEXTURE2D_DESC {
            Width: source.Width,
            Height: source.Height,
            MipLevels: 1,
            ArraySize: 1,
            Format: source.Format,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };

        let mut texture: Option<ID3D11Texture2D> = None;
        // SAFETY: creating a CPU-readable staging texture needs no initial data.
        unsafe {
            self.device
                .CreateTexture2D(&desc, None, Some(&mut texture))
                .context("ID3D11Device::CreateTexture2D(staging) failed")?;
        }
        self.staging = Some(texture.context("CreateTexture2D returned no texture")?);
        self.staging_desc = desc;
        Ok(())
    }

    /// Releases the duplication object (and keeps the capturer usable: the next
    /// [`Self::capture`] rebuilds it).
    pub fn release(&mut self) -> Result<()> {
        self.duplication = None;
        Ok(())
    }

    /// Recreates the duplication object on the retained output/device.
    fn rebuild_duplication(&mut self) -> Result<()> {
        // SAFETY: device and output are alive for the lifetime of `self`.
        let duplication = unsafe {
            let output1: IDXGIOutput1 = self
                .output
                .cast()
                .context("IDXGIOutput -> IDXGIOutput1 cast failed")?;
            output1
                .DuplicateOutput(&self.device)
                .context("DuplicateOutput failed while rebuilding duplication")?
        };
        // SAFETY: GetDesc on a live duplication object.
        self.rotation = unsafe { duplication.GetDesc() }.Rotation;
        self.duplication = Some(duplication);
        Ok(())
    }
}

impl std::fmt::Debug for DxgiCapturer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DxgiCapturer")
            .field("monitor", &self.monitor)
            .field("rotation", &self.rotation)
            .field("has_duplication", &self.duplication.is_some())
            .field("frames_acquired", &self.frames_acquired)
            .field("rebuilds", &self.rebuilds)
            .finish()
    }
}

fn is_primary(desc: &DXGI_OUTPUT_DESC) -> bool {
    // The primary monitor is the one whose upper-left corner sits at the
    // virtual-desktop origin (0,0).
    desc.DesktopCoordinates.left == 0 && desc.DesktopCoordinates.top == 0
}

/// Whether the given error chain contains a DXGI `ACCESS_LOST` HRESULT.
fn is_access_lost(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        cause
            .downcast_ref::<windows::core::Error>()
            .map(|win| win.code() == DXGI_ERROR_ACCESS_LOST)
            .unwrap_or(false)
    })
}

/// Reads dirty and move rectangles for the currently owned frame.
///
/// Metadata failures are not fatal: the pixel payload is still complete, so an
/// empty list (meaning "full frame") is returned if the metadata APIs fail.
fn read_frame_metadata(
    dup: &IDXGIOutputDuplication,
    frame_info: &DXGI_OUTDUPL_FRAME_INFO,
) -> Vec<Rect> {
    let total = frame_info.TotalMetadataBufferSize as usize;
    if total == 0 {
        return Vec::new();
    }

    let mut rects = Vec::new();

    // SAFETY: the frame is owned by the caller; buffers are sized from
    // TotalMetadataBufferSize (DXGI's upper bound for all metadata).
    unsafe {
        // Dirty rectangles.
        let rect_size = size_of::<RECT>();
        let mut dirty_buffer: Vec<RECT> = vec![RECT::default(); total / rect_size + 2];
        let mut required: u32 = 0;
        for _ in 0..2 {
            let capacity_bytes = (dirty_buffer.len() * rect_size) as u32;
            match dup.GetFrameDirtyRects(capacity_bytes, dirty_buffer.as_mut_ptr(), &mut required) {
                Ok(()) => {
                    let count = (required as usize / rect_size).min(dirty_buffer.len());
                    rects.extend(dirty_buffer[..count].iter().map(rect_from_win));
                    break;
                }
                Err(e) if e.code() == DXGI_ERROR_MORE_DATA => {
                    let needed = required as usize / rect_size + 1;
                    dirty_buffer.resize(needed, RECT::default());
                }
                Err(_) => break,
            }
        }

        // Move rectangles: the destination of every move is dirty in this frame.
        // (Per MSDN, move rects must be processed before dirty rects.)
        let move_size = size_of::<DXGI_OUTDUPL_MOVE_RECT>();
        let mut move_buffer: Vec<DXGI_OUTDUPL_MOVE_RECT> =
            vec![Default::default(); total / move_size + 2];
        let mut required: u32 = 0;
        for _ in 0..2 {
            let capacity_bytes = (move_buffer.len() * move_size) as u32;
            match dup.GetFrameMoveRects(capacity_bytes, move_buffer.as_mut_ptr(), &mut required) {
                Ok(()) => {
                    let count = (required as usize / move_size).min(move_buffer.len());
                    rects.extend(
                        move_buffer[..count]
                            .iter()
                            .map(|m| rect_from_win(&m.DestinationRect)),
                    );
                    break;
                }
                Err(e) if e.code() == DXGI_ERROR_MORE_DATA => {
                    let needed = required as usize / move_size + 1;
                    move_buffer.resize(needed, Default::default());
                }
                Err(_) => break,
            }
        }
    }

    rects
}

impl Drop for DxgiCapturer {
    fn drop(&mut self) {
        // Dropping the duplication object releases it; do it explicitly first so
        // the GPU/compositor resources are freed before the device goes away.
        self.duplication = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitors_finds_the_desktop() {
        let monitors = DxgiCapturer::monitors().expect("monitors() failed");
        assert!(
            !monitors.is_empty(),
            "expected at least one desktop-attached monitor"
        );
        for (index, monitor) in monitors.iter().enumerate() {
            assert_eq!(monitor.id, index as u32, "ids follow enumeration order");
            assert!(!monitor.name.is_empty(), "monitor name should not be empty");
            assert!(monitor.w > 0 && monitor.h > 0, "monitor must have an area");
        }
        assert!(
            monitors.iter().any(|m| m.primary),
            "exactly one monitor sits at the virtual-desktop origin"
        );
    }

    #[test]
    #[ignore = "requires a live, unlocked Windows desktop session"]
    fn captures_a_real_frame() {
        let mut capturer = DxgiCapturer::new(0).expect("creating a capturer failed");
        let monitor = capturer.monitor().clone();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut blank_frames = 0u32;
        loop {
            match capturer.capture(16).expect("capture() failed") {
                Some(frame) => {
                    // The frame has the physical desktop size of the monitor.
                    assert_eq!(frame.width, monitor.w);
                    assert_eq!(frame.height, monitor.h);
                    assert_eq!(
                        frame.data.len(),
                        frame.stride as usize * frame.height as usize
                    );
                    let packed = frame.to_packed();
                    assert_eq!(
                        packed.len(),
                        frame.packed_stride() as usize * frame.height as usize
                    );
                    let nonzero = packed.iter().step_by(97).filter(|&&b| b != 0).count();
                    if nonzero > 0 {
                        return;
                    }
                    // The first frame handed out by a fresh duplication object can
                    // be an all-zero placeholder; keep waiting for a real one.
                    blank_frames += 1;
                    assert!(
                        std::time::Instant::now() < deadline,
                        "only blank frames ({blank_frames}) were delivered within 10s"
                    );
                }
                None if std::time::Instant::now() >= deadline => {
                    panic!("no desktop frame was presented within 10s (idle desktop?)")
                }
                None => {}
            }
        }
    }
}
