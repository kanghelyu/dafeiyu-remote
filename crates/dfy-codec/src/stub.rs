// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 非 Windows 平台的 stub 实现：所有工厂函数返回明确的 “不支持” 错误。
//!
//! 这样上层 `dfy-codec` 的调用方（例如 `dfy-session`）可以跨平台编译，
//! 但在 Linux/macOS 上运行到编码路径时会得到清晰的中文错误信息。

use anyhow::{bail, Result};

use crate::{Codec, EncoderConfig, EncoderInfo, PixelFormatIn, VideoDecoder, VideoEncoder};

const UNSUPPORTED: &str =
    "dfy-codec: 当前平台不支持（Windows Media Foundation 仅在 Windows 上可用）";

pub(crate) fn create_encoder(
    _cfg: &EncoderConfig,
    _format: Option<PixelFormatIn>,
) -> Result<Box<dyn VideoEncoder>> {
    let _ = UNSUPPORTED;
    bail!("{UNSUPPORTED}")
}

pub(crate) fn create_decoder(_prefer_hardware: bool) -> Result<Box<dyn VideoDecoder>> {
    bail!("{UNSUPPORTED}")
}

pub(crate) fn available_encoders() -> Vec<EncoderInfo> {
    let _ = Codec::H264;
    Vec::new()
}
