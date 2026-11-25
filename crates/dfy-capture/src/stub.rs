//! 非 Windows 且非 Linux 平台的占位实现（目前实际只有 macOS 走到这里）。
//!
//! 目的只有一个：让整个 workspace 在这些平台上依然能 `cargo check`，
//! 同时把「当前平台没有采集后端」这件事变成可读的
//! [`CaptureError::Unsupported`]，而不是编译错误。

use crate::{CaptureError, CapturedFrame, MonitorInfo};

/// 占位采集器（macOS 等平台）。
///
/// 所有构造/方法都以 [`CaptureError::Unsupported`] 失败；类型存在只是为了让
/// 依赖 crate 仍能写出 `dfy_capture::DxgiCapturer` 并通过编译。
/// Linux 平台走 [`crate::LinuxCapturer`]，不会编译到本模块。
#[derive(Debug)]
pub struct DxgiCapturer {
    _private: (),
}

impl DxgiCapturer {
    /// 当前平台没有采集后端，固定失败。
    pub fn new(_monitor_index: u32) -> anyhow::Result<Self> {
        Err(CaptureError::Unsupported(
            "当前平台没有屏幕采集后端（Windows: DXGI，Linux: X11）；macOS 后端尚未实现",
        )
        .into())
    }

    /// 当前平台没有采集后端，固定失败。
    pub fn monitors() -> anyhow::Result<Vec<MonitorInfo>> {
        Err(CaptureError::Unsupported(
            "当前平台没有显示器枚举实现（Windows: DXGI，Linux: RandR/Xinerama）",
        )
        .into())
    }

    /// 当前平台没有采集后端，固定失败。
    pub fn capture(&mut self, _timeout_ms: u32) -> anyhow::Result<Option<CapturedFrame>> {
        Err(
            CaptureError::Unsupported("当前平台没有屏幕采集后端（Windows: DXGI，Linux: X11）")
                .into(),
        )
    }

    /// 空操作：没有需要释放的 duplication 对象。
    pub fn release(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}
