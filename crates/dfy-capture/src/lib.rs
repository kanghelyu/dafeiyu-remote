//! `dfy-capture` — screen capture backends for the Dafeiyu remote-desktop
//! project.
//!
//! * **Windows**: DXGI Desktop Duplication
//!   (`IDXGIOutput1::DuplicateOutput` + D3D11 staging-texture readback) grabs
//!   the desktop at the adapter's native resolution, GPU-side, without going
//!   through GDI.
//! * **Linux**: X11 `GetImage` through the pure-Rust `x11rb` protocol stack
//!   (no `libX11`/`libxcb` linkage). Monitors are enumerated with RandR
//!   (1.5 `GetMonitors`, falling back to 1.2 CRTC/Output), then Xinerama, then
//!   the root window. Wayland compositors that do not expose XWayland are
//!   *not* supported by this backend; see `design/025-linux-capture.md` for the
//!   planned Wayland (portal + PipeWire) route.
//! * **Other platforms** (macOS): a stub whose constructors return an
//!   [`Unsupported`](CaptureError::Unsupported) error, so that the rest of the
//!   workspace still `cargo check`s everywhere.
//!
//! # Threading
//!
//! The Windows backend object (`DxgiCapturer`) owns COM/D3D11 objects bound to
//! the thread that created them (and to the desktop that was active at that
//! time). It is deliberately **not `Send`**: create one instance per thread that
//! needs to capture, use it only from that thread, and drop it there. A single
//! instance per display is also required by DXGI itself (Desktop Duplication
//! allows only one active duplication object per output).
//!
//! The Linux backend object (`LinuxCapturer`) owns one X11 connection; it is
//! also intended to be used (and dropped) on a single capture thread, although
//! `x11rb`'s connection itself is `Send`.

#![warn(missing_docs)]

use std::fmt;

/// Pixel layout of the CPU-side frame bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PixelFormat {
    /// 4 bytes per pixel, byte order B, G, R, A (DXGI_FORMAT_B8G8R8A8_UNORM).
    Bgra8,
    /// 4 bytes per pixel, byte order R, G, B, A.
    Rgba8,
}

impl PixelFormat {
    /// Bytes per pixel for this format.
    pub const fn bytes_per_pixel(self) -> u32 {
        4
    }
}

impl fmt::Display for PixelFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PixelFormat::Bgra8 => f.write_str("Bgra8"),
            PixelFormat::Rgba8 => f.write_str("Rgba8"),
        }
    }
}

/// A rectangle in pixels, with `right`/`bottom` exclusive.
///
/// Coordinates are relative to the captured surface (0,0 = upper-left of the
/// monitor).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Rect {
    /// Left edge (inclusive).
    pub left: i32,
    /// Top edge (inclusive).
    pub top: i32,
    /// Right edge (exclusive).
    pub right: i32,
    /// Bottom edge (exclusive).
    pub bottom: i32,
}

impl Rect {
    /// Creates a rectangle from its edges.
    pub const fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    /// Width in pixels (never negative).
    pub const fn width(&self) -> u32 {
        let width = self.right - self.left;
        if width < 0 {
            0
        } else {
            width as u32
        }
    }

    /// Height in pixels (never negative).
    pub const fn height(&self) -> u32 {
        let height = self.bottom - self.top;
        if height < 0 {
            0
        } else {
            height as u32
        }
    }

    /// Whether the rectangle has no area.
    pub const fn is_empty(&self) -> bool {
        self.right <= self.left || self.bottom <= self.top
    }
}

impl fmt::Display for Rect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}x{}+{},{}",
            self.width(),
            self.height(),
            self.left,
            self.top
        )
    }
}

/// Description of one desktop output (monitor).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorInfo {
    /// Index of this monitor in the enumeration order; also the value to pass
    /// as `monitor_index` to the platform backend's `new`.
    pub id: u32,
    /// Device name as reported by the platform backend (DXGI `\\.\DISPLAY1` on
    /// Windows, the RandR/Xinerama monitor name on Linux).
    pub name: String,
    /// Virtual-desktop x coordinate of the monitor's upper-left corner.
    pub x: i32,
    /// Virtual-desktop y coordinate of the monitor's upper-left corner.
    pub y: i32,
    /// Width in physical pixels.
    pub w: u32,
    /// Height in physical pixels.
    pub h: u32,
    /// Whether this is the primary monitor (its origin is the virtual-desktop
    /// origin).
    pub primary: bool,
    /// Effective (DPI-scaled) dots per inch, or 0 if it could not be queried.
    pub dpi: u32,
}

/// One captured desktop frame, already copied to CPU memory.
#[derive(Debug, Clone)]
pub struct CapturedFrame {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Row pitch in bytes of [`Self::data`] (may contain padding).
    pub stride: u32,
    /// Pixel layout of [`Self::data`].
    pub format: PixelFormat,
    /// Raw pixel bytes, `stride` bytes per row, `height` rows.
    ///
    /// Use [`Self::to_packed`] to obtain tightly packed rows.
    pub data: Vec<u8>,
    /// Capture timestamp from `QueryPerformanceCounter`.
    pub timestamp_qpc: i64,
    /// Dirty rectangles reported by the compositor for this frame.
    ///
    /// An empty vector means "no partial-update information / treat as a full
    /// frame". (DXGI reports an empty metadata set when nothing moved; callers
    /// that require full redraws should honour `full_frame`.)
    pub dirty: Vec<Rect>,
    /// True when DXGI reported no dirty rectangles at all (typically a static
    /// desktop frame). The pixel payload is still valid either way.
    pub full_frame: bool,
}

impl CapturedFrame {
    /// Bytes per pixel of this frame.
    pub const fn bytes_per_pixel(&self) -> u32 {
        self.format.bytes_per_pixel()
    }

    /// Number of bytes in one packed (padding-free) row.
    pub const fn packed_stride(&self) -> u32 {
        self.width * self.format.bytes_per_pixel()
    }

    /// Returns the pixel bytes with row padding removed (`height * width * 4`
    /// bytes).
    ///
    /// If [`Self::data`] is already tightly packed this is a cheap copy; the
    /// method never mutates the frame.
    pub fn to_packed(&self) -> Vec<u8> {
        let bpp = self.format.bytes_per_pixel() as usize;
        let row = self.width as usize * bpp;
        let stride = self.stride as usize;
        if stride == row {
            return self.data.clone();
        }
        let mut out = Vec::with_capacity(row * self.height as usize);
        for y in 0..self.height as usize {
            let start = y * stride;
            // Defensive: a malformed stride must not panic in the middle of a
            // live capture loop.
            let end = (start + row).min(self.data.len());
            if start >= end {
                out.resize(out.len() + row, 0);
            } else {
                out.extend_from_slice(&self.data[start..end]);
                out.resize(out.len() + (row - (end - start)), 0);
            }
        }
        out
    }

    /// Returns each row of [`Self::data`] as a `&[u8]` slice, padding included.
    pub fn rows(&self) -> impl Iterator<Item = &[u8]> {
        let stride = self.stride as usize;
        self.data.chunks(stride.max(1)).take(self.height as usize)
    }
}

/// Errors produced by the capture backends.
#[derive(Debug)]
pub enum CaptureError {
    /// The requested monitor index does not exist.
    NoSuchMonitor(u32),
    /// Desktop Duplication is not available on this platform.
    Unsupported(&'static str),
    /// Any other failure.
    Other(anyhow::Error),
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CaptureError::NoSuchMonitor(i) => write!(f, "no such monitor index: {i}"),
            CaptureError::Unsupported(what) => write!(f, "unsupported: {what}"),
            CaptureError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for CaptureError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CaptureError::Other(e) => Some(e.as_ref()),
            _ => None,
        }
    }
}

impl From<anyhow::Error> for CaptureError {
    fn from(e: anyhow::Error) -> Self {
        CaptureError::Other(e)
    }
}

#[cfg(windows)]
mod dxgi;
#[cfg(windows)]
pub use dxgi::DxgiCapturer;

// Linux 走 X11 后端（`linux.rs`）；只有在既非 Windows 又非 Linux 的平台
// （目前是 macOS）才退化成 stub，保证整个 workspace 仍可编译。
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::LinuxCapturer;

#[cfg(not(any(windows, target_os = "linux")))]
mod stub;
#[cfg(not(any(windows, target_os = "linux")))]
pub use stub::DxgiCapturer;

/// The crate version, for handshake/diagnostics messages.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(width: u32, height: u32, stride: u32) -> CapturedFrame {
        let mut data = vec![0u8; stride as usize * height as usize];
        for y in 0..height as usize {
            for x in 0..(width as usize * 4) {
                data[y * stride as usize + x] = (y as u8) << 4 | (x as u8 & 0x0f);
            }
            // Padding bytes carry junk; to_packed must drop them.
            for x in (width as usize * 4)..stride as usize {
                data[y * stride as usize + x] = 0xEE;
            }
        }
        CapturedFrame {
            width,
            height,
            stride,
            format: PixelFormat::Bgra8,
            data,
            timestamp_qpc: 1,
            dirty: Vec::new(),
            full_frame: true,
        }
    }

    #[test]
    fn to_packed_strips_row_padding() {
        let f = frame(3, 2, 16);
        let packed = f.to_packed();
        assert_eq!(packed.len(), 3 * 4 * 2);
        assert_eq!(&packed[0..12], &f.data[0..12]);
        assert_eq!(&packed[12..24], &f.data[16..28]);
        assert!(!packed.contains(&0xEE));
    }

    #[test]
    fn to_packed_is_identity_when_already_packed() {
        let f = frame(4, 4, 16);
        assert_eq!(f.to_packed(), f.data);
    }

    #[test]
    fn rect_helpers() {
        let r = Rect::new(10, 20, 30, 50);
        assert_eq!(r.width(), 20);
        assert_eq!(r.height(), 30);
        assert!(!r.is_empty());
        assert_eq!(r.to_string(), "20x30+10,20");
        assert!(Rect::new(5, 5, 5, 9).is_empty());
        assert!(Rect::new(9, 5, 5, 9).is_empty());
    }
}
