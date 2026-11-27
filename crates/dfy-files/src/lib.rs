// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! # dfy-files — 文件传输状态机（与网络解耦）
//!
//! 本 crate 只负责「拿到一条协议消息 → 推进状态 → 产出下一条协议消息」，
//! 不持有 QUIC 连接、不做异步 I/O、不关心 control 流的具体收发方式：
//! 由 `dfy-session` 的 [`CtrlHandler`] 风格回调把 [`Envelope`] 喂进来/取出去。
//! 好处是状态机可以纯同步单测（`tempfile` + 真文件），也便于未来换到
//! `file-data` 独立单向流。
//!
//! ## 组成
//!
//! - [`FileSender`]：`offer → on_accept(seek) → next_chunk* → on_ack* → on_result`；
//! - [`FileReceiver`]：`on_offer(净化 + 策略 + 续传) → on_chunk* (ack/result)`；
//! - [`plan_directory`]：目录递归展开成确定序的文件/目录清单（跳过符号链接）；
//! - [`sanitize_relative_path`]：协议路径净化（拒绝 `..`/绝对路径/Windows 设备名/符号链接逃逸）；
//! - [`FileError`]：可判别的错误变体（`InvalidPath`/`ResumeMismatch`/`Io` 等）。
//!
//! ## 一次完整传输（发送端视角）
//!
//! ```text
//! offer(path) ──► FileOffer ─────────────► 接收端
//!                                        │
//! on_accept(FileAccept{resume_offset}) ◄─┘
//!   │ seek(resume_offset)，续传前缀补算 BLAKE3
//!   ▼
//! next_chunk(256KiB) ──► FileChunk*（受 16MiB 在途窗口背压）
//! on_ack(FileAck{received}) ◄──────────── 每 16MiB / 64 块一次
//! on_result(FileResult{ok, digest}) ◄──── 最后一块后：flush + 校验大小 + BLAKE3
//! ```
//!
//! 接收端以**已落盘字节数**作为续传起点（不信任发送端声明），先写
//! `<name>.dfy-part`，校验通过后 rename 到最终路径（原子落盘）；失败/中断
//! 时保留 part 文件供下次续传。
//!
//! [`Envelope`]: dfy_proto::Envelope
//! [`CtrlHandler`]: https://docs.rs/anyhow

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::path::Path;

mod error;
mod path;
mod plan;
mod receiver;
mod sender;

pub use error::FileError;
pub use path::{sanitize_file_name, sanitize_relative_path};
pub use plan::{plan_directory, plan_directory_with, DirEntryPlan, PlanOptions};
pub use receiver::{FileReceiveStats, FileReceiver, OverwritePolicy, ReceiveConfig};
pub use sender::{FileSender, FileTransferStats, SenderConfig};

/// 默认数据块大小：256 KiB（`spec/DFYP-v1.md` §9 建议值）。
pub const DEFAULT_CHUNK_BYTES: usize = 256 * 1024;

/// 默认流控确认间隔：16 MiB。
pub const DEFAULT_ACK_BYTES: u64 = 16 * 1024 * 1024;

/// 默认流控确认间隔：64 块。
pub const DEFAULT_ACK_CHUNKS: u32 = 64;

/// 发送端默认在途未确认窗口：16 MiB（与 ack 间隔对齐，避免接收端内存无界增长）。
pub const DEFAULT_IN_FLIGHT_BYTES: u64 = 16 * 1024 * 1024;

/// BLAKE3 摘要长度（字节）。
pub const DIGEST_BYTES: usize = 32;

/// 接收端临时文件名后缀：先写 `.dfy-part`，校验通过后 rename 到最终路径。
pub const PART_SUFFIX: &str = ".dfy-part";

/// 构造 `<target>.dfy-part` 路径（同目录，不新增路径组件）。
pub(crate) fn part_path(target: &Path) -> std::path::PathBuf {
    let mut os = target.as_os_str().to_os_string();
    os.push(PART_SUFFIX);
    std::path::PathBuf::from(os)
}
