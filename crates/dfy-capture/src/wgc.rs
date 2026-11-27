//! Windows Graphics Capture monitor backend.

use std::marker::PhantomData;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use windows::core::Interface;
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{HMODULE, RPC_E_CHANGED_MODE};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_10_0,
    D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D,
    D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::{Common::DXGI_FORMAT_B8G8R8A8_UNORM, IDXGIDevice};
use windows::Win32::System::Performance::QueryPerformanceCounter;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED};

use crate::{CapturedFrame, MonitorInfo, PixelFormat};

const PIXEL_FORMAT: DirectXPixelFormat = DirectXPixelFormat::B8G8R8A8UIntNormalized;
const FRAME_POOL_BUFFERS: i32 = 2;
const FRAME_POLL_INTERVAL: Duration = Duration::from_millis(2);

struct RoApartmentGuard(bool);

impl Drop for RoApartmentGuard {
    fn drop(&mut self) {
        if self.0 {
            unsafe { RoUninitialize() };
        }
    }
}

fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let feature_levels = [
        D3D_FEATURE_LEVEL_11_0,
        D3D_FEATURE_LEVEL_10_1,
        D3D_FEATURE_LEVEL_10_0,
    ];
    let mut last_error = None;
    for driver_type in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
        let mut device = None;
        let mut context = None;
        let mut feature_level = D3D_FEATURE_LEVEL::default();
        let result = unsafe {
            D3D11CreateDevice(
                None,
                driver_type,
                HMODULE(std::ptr::null_mut()),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&feature_levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut feature_level),
                Some(&mut context),
            )
        };
        match result {
            Ok(()) => {
                return Ok((
                    device.context("D3D11CreateDevice returned no device")?,
                    context.context("D3D11CreateDevice returned no context")?,
                ));
            }
            Err(error) => last_error = Some(error),
        }
    }
    Err(anyhow!(last_error.context("no D3D11 device available")?))
        .context("creating a D3D11 device for Windows Graphics Capture failed")
}

fn staging_texture(device: &ID3D11Device, width: u32, height: u32) -> Result<ID3D11Texture2D> {
    anyhow::ensure!(
        width > 0 && height > 0,
        "WGC returned an empty content size"
    );
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut texture = None;
    unsafe {
        device
            .CreateTexture2D(&desc, None, Some(&mut texture))
            .context("creating WGC staging texture failed")?;
    }
    texture.context("CreateTexture2D returned no WGC staging texture")
}

fn qpc_ticks() -> i64 {
    let mut ticks = 0i64;
    unsafe {
        let _ = QueryPerformanceCounter(&mut ticks);
    }
    ticks
}

/// A monitor capturer backed by Windows Graphics Capture.
///
/// WGC uses the compositor rather than Desktop Duplication, so it also works
/// with virtual outputs that never publish DXGI duplication frames.
pub struct WgcCapturer {
    _item: GraphicsCaptureItem,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    direct_device: IDirect3DDevice,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    staging: ID3D11Texture2D,
    size: windows::Graphics::SizeInt32,
    monitor: MonitorInfo,
    _not_send: PhantomData<*mut ()>,
    _ro_apartment: Option<RoApartmentGuard>,
}

impl WgcCapturer {
    /// Creates a WGC capturer for the monitor at `monitor_index`.
    pub fn new(monitor_index: u32) -> Result<Self> {
        let (mut monitor, hmonitor) = crate::dxgi::select_monitor(monitor_index)?;
        let ro_apartment = match unsafe { RoInitialize(RO_INIT_MULTITHREADED) } {
            Ok(()) => Some(RoApartmentGuard(true)),
            Err(error) if error.code() == RPC_E_CHANGED_MODE => None,
            Err(error) => return Err(error).context("RoInitialize(MTA) failed"),
        };
        (|| {
            let item = unsafe {
                let interop =
                    windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
                interop
                    .CreateForMonitor::<GraphicsCaptureItem>(hmonitor)
                    .context("creating WGC monitor item failed")?
            };
            let size = item.Size().context("querying WGC monitor size failed")?;
            monitor.w = u32::try_from(size.Width).context("WGC width exceeds u32")?;
            monitor.h = u32::try_from(size.Height).context("WGC height exceeds u32")?;
            let (device, context) = create_device()?;
            let dxgi_device = device.cast::<IDXGIDevice>()?;
            let direct_device = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device) }
                .context("wrapping D3D11 device for WGC failed")?
                .cast::<IDirect3DDevice>()?;
            let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
                &direct_device,
                PIXEL_FORMAT,
                FRAME_POOL_BUFFERS,
                size,
            )
            .context("creating WGC frame pool failed")?;
            let session = pool
                .CreateCaptureSession(&item)
                .context("creating WGC capture session failed")?;
            let staging = staging_texture(&device, size.Width as u32, size.Height as u32)?;
            session
                .StartCapture()
                .context("starting WGC capture session failed")?;
            Ok(Self {
                _item: item,
                device,
                context,
                direct_device,
                pool,
                session,
                staging,
                size,
                monitor,
                _not_send: PhantomData,
                _ro_apartment: ro_apartment,
            })
        })()
    }

    /// Metadata for the monitor this capturer was created for.
    pub fn monitor(&self) -> &MonitorInfo {
        &self.monitor
    }

    /// Captures the newest available compositor frame, waiting up to `timeout_ms`.
    pub fn capture(&mut self, timeout_ms: u32) -> Result<Option<CapturedFrame>> {
        let deadline = Instant::now() + Duration::from_millis(u64::from(timeout_ms));
        loop {
            let mut latest = None;
            loop {
                match self.pool.TryGetNextFrame() {
                    Ok(frame) => {
                        if let Some(previous) = latest.replace(frame) {
                            let _ = previous.Close();
                        }
                    }
                    Err(error) if error.code().0 == 0 => break,
                    Err(error) => return Err(error).context("WGC TryGetNextFrame failed"),
                }
            }
            if let Some(frame) = latest {
                let frame = self.read_frame(frame)?;
                return Ok(Some(CapturedFrame {
                    width: frame.width,
                    height: frame.height,
                    stride: frame.stride,
                    format: PixelFormat::Bgra8,
                    data: frame.data,
                    timestamp_qpc: qpc_ticks(),
                    dirty: Vec::new(),
                    full_frame: true,
                }));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            thread::sleep(remaining.min(FRAME_POLL_INTERVAL));
        }
    }

    fn read_frame(&mut self, frame: Direct3D11CaptureFrame) -> Result<crate::CapturedFrame> {
        let result = (|| {
            let content = frame
                .ContentSize()
                .context("querying WGC content size failed")?;
            let width = u32::try_from(content.Width).context("WGC width exceeds u32")?;
            let height = u32::try_from(content.Height).context("WGC height exceeds u32")?;
            if content != self.size {
                self.staging = staging_texture(&self.device, width, height)?;
                self.pool
                    .Recreate(
                        &self.direct_device,
                        PIXEL_FORMAT,
                        FRAME_POOL_BUFFERS,
                        content,
                    )
                    .context("resizing WGC frame pool failed")?;
                self.size = content;
                self.monitor.w = width;
                self.monitor.h = height;
            }
            let surface = frame
                .Surface()
                .context("getting WGC frame surface failed")?;
            let access = surface
                .cast::<IDirect3DDxgiInterfaceAccess>()
                .context("casting WGC surface to DXGI interface access failed")?;
            let source = unsafe { access.GetInterface::<ID3D11Texture2D>() }
                .context("getting WGC frame as ID3D11Texture2D failed")?;
            let staging_resource = self.staging.cast::<ID3D11Resource>()?;
            let source_resource = source.cast::<ID3D11Resource>()?;
            let copy_box = D3D11_BOX {
                left: 0,
                top: 0,
                front: 0,
                right: width,
                bottom: height,
                back: 1,
            };
            unsafe {
                self.context.CopySubresourceRegion(
                    &staging_resource,
                    0,
                    0,
                    0,
                    0,
                    &source_resource,
                    0,
                    Some(&copy_box),
                );
            }
            let row_bytes = width
                .checked_mul(4)
                .context("WGC row size overflow")?
                .try_into()
                .context("WGC row size exceeds usize")?;
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            unsafe {
                self.context
                    .Map(&staging_resource, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                    .context("mapping WGC staging texture failed")?;
            }
            let readback = (|| {
                anyhow::ensure!(
                    !mapped.pData.is_null() && mapped.RowPitch as usize >= row_bytes,
                    "WGC returned an invalid mapped row"
                );
                let len = row_bytes
                    .checked_mul(height as usize)
                    .context("WGC frame size overflow")?;
                let mut data = Vec::with_capacity(len);
                for row in 0..height as usize {
                    let start = row * mapped.RowPitch as usize;
                    let bytes = unsafe {
                        std::slice::from_raw_parts(
                            (mapped.pData as *const u8).add(start),
                            row_bytes,
                        )
                    };
                    data.extend_from_slice(bytes);
                }
                Ok(data)
            })();
            unsafe {
                self.context.Unmap(&staging_resource, 0);
            }
            let data = readback?;
            Ok(crate::CapturedFrame {
                width,
                height,
                stride: width * 4,
                format: PixelFormat::Bgra8,
                data,
                timestamp_qpc: qpc_ticks(),
                dirty: Vec::new(),
                full_frame: true,
            })
        })();
        let _ = frame.Close();
        result
    }
}

impl Drop for WgcCapturer {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

/// Windows capture backend with WGC preferred and DXGI as an initialization fallback.
pub struct WindowsCapturer {
    backend: WindowsCaptureBackend,
}

enum WindowsCaptureBackend {
    Wgc(WgcCapturer),
    Dxgi(crate::DxgiCapturer),
}

impl WindowsCapturer {
    /// Creates a capturer for the monitor at `monitor_index`.
    pub fn new(monitor_index: u32) -> Result<Self> {
        match WgcCapturer::new(monitor_index) {
            Ok(capturer) => Ok(Self {
                backend: WindowsCaptureBackend::Wgc(capturer),
            }),
            Err(wgc_error) => {
                let capturer = crate::DxgiCapturer::new(monitor_index).with_context(|| {
                    format!("WGC initialization failed ({wgc_error:#}); DXGI fallback also failed")
                })?;
                Ok(Self {
                    backend: WindowsCaptureBackend::Dxgi(capturer),
                })
            }
        }
    }

    /// Lists desktop-attached monitors in capture-index order.
    pub fn monitors() -> Result<Vec<MonitorInfo>> {
        crate::DxgiCapturer::monitors()
    }

    /// Metadata for the selected monitor.
    pub fn monitor(&self) -> &MonitorInfo {
        match &self.backend {
            WindowsCaptureBackend::Wgc(capturer) => capturer.monitor(),
            WindowsCaptureBackend::Dxgi(capturer) => capturer.monitor(),
        }
    }

    /// Captures the next available frame.
    pub fn capture(&mut self, timeout_ms: u32) -> Result<Option<CapturedFrame>> {
        match &mut self.backend {
            WindowsCaptureBackend::Wgc(capturer) => capturer.capture(timeout_ms),
            WindowsCaptureBackend::Dxgi(capturer) => capturer.capture(timeout_ms),
        }
    }

    /// Name of the active backend for diagnostics.
    pub fn backend_name(&self) -> &'static str {
        match self.backend {
            WindowsCaptureBackend::Wgc(_) => "WGC",
            WindowsCaptureBackend::Dxgi(_) => "DXGI",
        }
    }
}

impl Drop for WindowsCapturer {
    fn drop(&mut self) {
        if let WindowsCaptureBackend::Dxgi(capturer) = &mut self.backend {
            let _ = capturer.release();
        }
    }
}

impl std::fmt::Debug for WindowsCapturer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowsCapturer")
            .field("backend", &self.backend_name())
            .field("monitor", self.monitor())
            .finish()
    }
}
