// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// 大肥鱼远程 —— 非 Windows 平台的剪贴板后端占位实现。
//
// macOS / Linux 后端会在后续里程碑按同一 `ClipboardBackend` trait 补齐
// （macOS: NSPasteboard + changeCount；Linux: X11 selections / wl-clipboard）。

// 当前仅 Windows 后端/测试使用；其它平台编译时允许未使用（后续 mac/Linux 后端会复用）。
#![cfg_attr(not(windows), allow(dead_code))]

use anyhow::bail;
use anyhow::Result;

use crate::{ClipKind, ClipboardBackend, DataBlock, FormatList};

/// 非 Windows 平台：创建后端直接返回「不支持」。
pub fn create_backend() -> Result<Box<dyn ClipboardBackend>> {
    bail!("当前平台尚未实现剪贴板后端（macOS/Linux 在后续里程碑补齐）")
}

/// 占位后端：所有方法返回「不支持」，供需要构造类型的调用方使用。
#[derive(Debug, Default)]
pub struct UnsupportedBackend;

impl ClipboardBackend for UnsupportedBackend {
    fn sequence(&self) -> Result<u64> {
        bail!("当前平台不支持剪贴板序列号")
    }

    fn list_formats(&self) -> Result<FormatList> {
        bail!("当前平台不支持剪贴板格式枚举")
    }

    fn read_block(&self, _format_id: u32, _offset: u64, _max: usize) -> Result<DataBlock> {
        bail!("当前平台不支持剪贴板读取")
    }

    fn write(
        &mut self,
        _kind: &ClipKind,
        _bytes: &[u8],
        _files: &[std::path::PathBuf],
    ) -> Result<()> {
        bail!("当前平台不支持剪贴板写入")
    }
}
