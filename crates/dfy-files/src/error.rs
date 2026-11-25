// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 可判别的错误类型：调用方按变体分类处理（拒绝传输 / 终止会话 / 重试）。

use std::fmt;
use std::io;
use std::path::Path;

/// 文件传输错误。
///
/// 设计原则：**协议层可预期失败**（非法路径、偏移不符、摘要不符…）都有独立变体，
/// 便于 `dfy-session` 决定是「回 FileResult 失败」「断开传输」还是「终止会话」；
/// 只有 I/O 细节放进 [`FileError::Io`]。
#[derive(Debug, thiserror::Error)]
pub enum FileError {
    /// 路径净化失败：含 `..`、绝对路径/盘符、Windows 设备名、控制字符、符号链接逃逸等。
    #[error("非法路径: {0}")]
    InvalidPath(String),
    /// 续传偏移与本地文件/声明大小不符。
    #[error("续传偏移不匹配: 期望 {expected} 字节, 实际 {got} 字节")]
    ResumeMismatch {
        /// 期望值（文件大小 / 接收端已落盘长度）。
        expected: u64,
        /// 实际收到或观察到的值。
        got: u64,
    },
    /// 分块 `offset` 与接收端期望值不符（缺口），或与已有数据重叠。
    #[error("分块乱序: 期望 offset={expected}, 收到 offset={got}")]
    ChunkOutOfOrder {
        /// 接收端期望的下一个 offset。
        expected: u64,
        /// 消息携带的 offset。
        got: u64,
    },
    /// `transfer_id` 与当前传输不符（串台/过期消息）。
    #[error("传输 ID 不匹配: 当前 {expected}, 收到 {got}")]
    TransferMismatch {
        /// 当前状态机正在处理的 ID。
        expected: u64,
        /// 消息携带的 ID。
        got: u64,
    },
    /// 状态机调用顺序错误（例如没有 `FileOffer` 就 `on_accept`）。
    #[error("状态错误: {0}")]
    BadState(&'static str),
    /// 对端报告传输失败（`FileResult.ok == false`）。
    #[error("对端报告传输失败: {0}")]
    TransferFailed(String),
    /// 发送端自算 BLAKE3 与接收端回报的摘要不一致（内容损坏/被篡改）。
    #[error("BLAKE3 摘要不一致: 本地 {local}, 对端 {remote}")]
    DigestMismatch {
        /// 发送端自算摘要（hex）。
        local: String,
        /// 接收端回报摘要（hex）。
        remote: String,
    },
    /// 实际字节数与声明大小不符。
    #[error("大小不符: 声明 {declared} 字节, 实际 {actual} 字节")]
    SizeMismatch {
        /// offer 声明的大小。
        declared: u64,
        /// 实际观察到的大小。
        actual: u64,
    },
    /// 目标路径类型冲突（期望文件却是目录，或反之），拒绝破坏性覆盖。
    #[error("目标类型冲突: {path}")]
    TargetTypeMismatch {
        /// 冲突的目标路径。
        path: String,
    },
    /// 数据块超过配置上限（防御恶意/错误实现产生的超大块）。
    #[error("数据块过大: {len} 字节 > 上限 {max} 字节")]
    ChunkTooLarge {
        /// 实际块大小。
        len: usize,
        /// 允许上限。
        max: usize,
    },
    /// 对端声明的文件大小超过接收端上限（防恶意 offer 打满磁盘）。
    #[error("文件过大: 声明 {declared} 字节 > 上限 {max} 字节")]
    FileTooLarge {
        /// offer 声明的大小。
        declared: u64,
        /// 接收端允许的单文件上限。
        max: u64,
    },
    /// 配置参数非法。
    #[error("配置非法: {0}")]
    Config(&'static str),
    /// I/O 失败（带路径上下文，便于日志定位）。
    #[error("I/O 失败({path}): {source}")]
    Io {
        /// 出错的路径。
        path: String,
        /// 底层 I/O 错误。
        #[source]
        source: io::Error,
    },
}

impl FileError {
    /// 构造带路径上下文的 I/O 错误。
    pub(crate) fn io(path: &Path, source: io::Error) -> Self {
        Self::Io {
            path: path.display().to_string(),
            source,
        }
    }

    /// 路径净化错误的便捷构造（统一加前缀，便于日志）。
    pub(crate) fn invalid(reason: impl fmt::Display) -> Self {
        Self::InvalidPath(reason.to_string())
    }
}
