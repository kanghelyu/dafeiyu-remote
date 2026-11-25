// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 非 Windows 平台的占位实现。
//!
//! mac/Linux 后端（CGEvent / XTest / uinput）尚未实现；`create_sink` 返回明确的
//! [`crate::InputError::Unsupported`]，避免静默失败。接口保持 trait 形状不变，
//! 后续只需替换本模块即可。

use crate::event::{InputError, InputSink};

/// 创建注入后端（非 Windows 暂不支持）。
pub(crate) fn create_sink() -> anyhow::Result<Box<dyn InputSink>> {
    Err(InputError::Unsupported.into())
}
