//! Non-Windows stub: keeps the crate compiling (and gives clear errors) on
//! platforms without DXGI Desktop Duplication.

use crate::{CaptureError, CapturedFrame, MonitorInfo};

/// Stub capture object for non-Windows platforms.
///
/// Every constructor/method fails with [`CaptureError::Unsupported`]; the type
/// exists only so that dependent crates can name and `cargo check` it on
/// Linux/macOS.
#[derive(Debug)]
pub struct DxgiCapturer {
    _private: (),
}

impl DxgiCapturer {
    /// Always fails on non-Windows platforms.
    pub fn new(_monitor_index: u32) -> anyhow::Result<Self> {
        Err(
            CaptureError::Unsupported("DXGI Desktop Duplication is only available on Windows")
                .into(),
        )
    }

    /// Always fails on non-Windows platforms.
    pub fn monitors() -> anyhow::Result<Vec<MonitorInfo>> {
        Err(
            CaptureError::Unsupported("DXGI monitor enumeration is only available on Windows")
                .into(),
        )
    }

    /// Always fails on non-Windows platforms.
    pub fn capture(&mut self, _timeout_ms: u32) -> anyhow::Result<Option<CapturedFrame>> {
        Err(
            CaptureError::Unsupported("DXGI Desktop Duplication is only available on Windows")
                .into(),
        )
    }

    /// No-op: nothing was duplicated.
    pub fn release(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}
