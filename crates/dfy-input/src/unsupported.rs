// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 尚未实现的平台后端。
//!
//! macOS/其他平台尚未实现；`create_sink` 返回明确的
//! [`crate::InputError::Unsupported`]，避免静默失败。

use crate::event::{InputError, InputSink};

/// 创建注入后端（当前平台暂不支持）。
pub(crate) fn create_sink() -> anyhow::Result<Box<dyn InputSink>> {
    Err(InputError::Unsupported.into())
}
