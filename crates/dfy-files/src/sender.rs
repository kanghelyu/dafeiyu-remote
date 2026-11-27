// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 发送端状态机：读文件 → 按块产出 [`Envelope::file_chunk`]。
//!
//! 状态流转：
//!
//! ```text
//! Idle ──offer()──► AwaitAccept ──on_accept(accept=false)──► Rejected
//!                        │
//!                        └──on_accept(resume_offset)──► Sending
//!                                                        │ next_chunk() × N
//!                                                        │ （16MiB 在途窗口背压）
//!                                                        └──on_result()──► Finished
//! ```
//!
//! 目录传输时 `offer(dir)` 会先用 [`plan_directory_with`] 展开清单放进 `pending`，
//! 目录 offer 被接受后调用 [`FileSender::next_offer`] 逐项 offer（目录项也会 offer，
//! 由接收端 `create_dir_all` 重建空目录）。发送端**不整文件读入内存**：每次只读
//! 一个块，摘要用流式 BLAKE3。

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use dfy_proto::{Envelope, FileAccept, FileAck, FileOffer, FileResult};

use crate::plan::{mtime_ms, plan_directory_with, PlanOptions};
use crate::{FileError, DEFAULT_CHUNK_BYTES, DEFAULT_IN_FLIGHT_BYTES, DIGEST_BYTES};

/// 发送端配置。
#[derive(Clone, Copy, Debug)]
pub struct SenderConfig {
    /// 单块上限（默认 256 KiB）；`next_chunk(max_bytes)` 传入更小值时以更小者为准。
    pub max_chunk_bytes: usize,
    /// 未确认在途字节窗口（默认 16 MiB）：超过后 `next_chunk` 返回 `None`，等 `FileAck`。
    pub in_flight_bytes: u64,
}

impl Default for SenderConfig {
    fn default() -> Self {
        Self {
            max_chunk_bytes: DEFAULT_CHUNK_BYTES,
            in_flight_bytes: DEFAULT_IN_FLIGHT_BYTES,
        }
    }
}

/// 发送端统计（每次 offer / next_offer 重置）。
#[derive(Clone, Debug, Default)]
pub struct FileTransferStats {
    /// 本次传输实际发出的文件负载字节（续传时不含已跳过的前缀）。
    pub bytes_sent: u64,
    /// 发出的 `FileChunk` 条数（含空 last 块）。
    pub chunks_sent: u64,
    /// 续传起始偏移（0 表示从头传）。
    pub resumed_from: u64,
    /// 发送端流式计算的整文件 BLAKE3（发出 last 块后填充）。
    pub digest: Option<[u8; DIGEST_BYTES]>,
}

/// 目录展开后待逐个 offer 的条目。
struct PendingItem {
    /// 协议中的相对路径（已带目录名前缀，例如 `photos/a.jpg`）。
    offer_name: String,
    /// 本机真实路径（只在本端使用，不上线）。
    path: PathBuf,
    size: u64,
    is_dir: bool,
    last_modified_ms: u64,
}

/// 正在传输的文件。
struct ActiveSend {
    transfer_id: u64,
    /// 出错时提供路径上下文。
    path: PathBuf,
    size: u64,
    file: File,
    /// 已发出的绝对偏移（= 下一条 chunk 的 offset）。
    sent: u64,
    /// 对端已确认落盘的绝对偏移（流控窗口起点）。
    acked: u64,
    /// 流式 BLAKE3（续传时先补算前缀）。
    hasher: blake3::Hasher,
    /// 是否已经发出 `last=true` 的块（之后 `next_chunk` 一律 `None`）。
    last_emitted: bool,
}

/// 状态机内部状态。
enum SenderState {
    /// 空闲（可以开始新的传输）。
    Idle,
    /// 已发 `FileOffer`，等待 `FileAccept`。
    AwaitAccept { offer: FileOffer, path: PathBuf },
    /// 已接受，正在发块；`Box` 避免 enum 因 BLAKE3 状态体过大。
    Sending(Box<ActiveSend>),
    /// 传输完成（收到对端 `FileResult` 或目录 offer 被接受）。
    Finished { transfer_id: u64, ok: bool },
    /// 对端拒绝（`accept=false`，如目标已存在且策略为 Skip）。
    Rejected { transfer_id: u64 },
}

/// 发送端：与网络解耦的文件发送状态机。
///
/// 典型调用（由 `dfy-session::CtrlHandler` 驱动）：
///
/// ```no_run
/// use std::path::Path;
/// use dfy_files::FileSender;
/// #
/// # fn demo(ctx: &mut ()) -> Result<(), dfy_files::FileError> {
/// let mut sender = FileSender::new(1);
/// let offer = sender.offer(Path::new("D:/tmp/big.bin"))?;
/// // 发出 Envelope::file_offer(...)，收到 FileAccept 后：
/// // let out = sender.on_accept(&accept)?;
/// // loop { match sender.next_chunk(256 * 1024)? { Some(env) => { /* 发出去，回程喂 ack */ } None => break } }
/// # let _ = (offer, ctx); Ok(()) }
/// ```
pub struct FileSender {
    next_transfer_id: u64,
    config: SenderConfig,
    state: SenderState,
    pending: VecDeque<PendingItem>,
    stats: FileTransferStats,
}

impl FileSender {
    /// 用 `transfer_id_base` 作为第一条传输的 ID（之后每 offer 一次自增 1）。
    pub fn new(transfer_id_base: u64) -> Self {
        Self::with_config(transfer_id_base, SenderConfig::default())
    }

    /// 带配置构造（块大小 / 在途窗口）。
    pub fn with_config(transfer_id_base: u64, config: SenderConfig) -> Self {
        Self {
            next_transfer_id: transfer_id_base,
            config,
            state: SenderState::Idle,
            pending: VecDeque::new(),
            stats: FileTransferStats::default(),
        }
    }

    /// 发起一次传输：文件直接读元数据；目录展开成清单并返回 `is_dir=true` 的 offer。
    ///
    /// 协议里只上送**文件名**（或目录展开后的相对路径），绝不上送本机绝对路径。
    /// 路径本身（文件名）必须先通过 [`crate::sanitize_file_name`]。
    pub fn offer(&mut self, path: &Path) -> Result<FileOffer, FileError> {
        if !self.pending.is_empty() {
            return Err(FileError::BadState(
                "目录展开队列尚未清空：请先 next_offer 或 clear_pending",
            ));
        }
        if !self.can_start() {
            return Err(FileError::BadState("当前传输尚未结束"));
        }
        let md = fs::metadata(path).map_err(|err| FileError::io(path, err))?;
        let name = path_to_offer_name(path)?;
        let last_modified_ms = mtime_ms(&md);
        let is_dir = md.is_dir();
        if !is_dir && !md.is_file() {
            return Err(FileError::invalid(format!(
                "不支持的文件类型（非普通文件/目录）: {}",
                path.display()
            )));
        }

        if is_dir {
            // 目录：先展开，随后由调用方逐个 next_offer（目录项也会 offer，用于重建空目录）。
            let plan = plan_directory_with(path, PlanOptions::default())?;
            self.pending = plan
                .into_iter()
                .map(|entry| PendingItem {
                    offer_name: format!("{name}/{}", entry.relative),
                    path: path.join(&entry.relative),
                    size: entry.size,
                    is_dir: entry.is_dir,
                    last_modified_ms: entry.last_modified_ms,
                })
                .collect();
        }

        let offer = FileOffer {
            transfer_id: self.alloc_transfer_id(),
            name,
            size: if is_dir { 0 } else { md.len() },
            last_modified_ms,
            is_dir,
        };
        self.stats = FileTransferStats::default();
        self.state = SenderState::AwaitAccept {
            offer: offer.clone(),
            path: path.to_path_buf(),
        };
        Ok(offer)
    }

    /// 目录传输专用：取出下一个待 offer 的条目（目录项与文件项都会返回）。
    ///
    /// 返回 `None` 表示目录清单已全部发出。调用方在收到 `FileResult`（文件）
    /// 或目录项被接受后，再次调用本方法。
    pub fn next_offer(&mut self) -> Result<Option<FileOffer>, FileError> {
        if matches!(
            &self.state,
            SenderState::AwaitAccept { .. } | SenderState::Sending(_)
        ) {
            return Err(FileError::BadState("上一次 offer 尚未接收/完成"));
        }
        let Some(item) = self.pending.pop_front() else {
            return Ok(None);
        };
        let offer = FileOffer {
            transfer_id: self.alloc_transfer_id(),
            name: item.offer_name,
            size: item.size,
            last_modified_ms: item.last_modified_ms,
            is_dir: item.is_dir,
        };
        self.stats = FileTransferStats::default();
        self.state = SenderState::AwaitAccept {
            offer: offer.clone(),
            path: item.path,
        };
        Ok(Some(offer))
    }

    /// 处理对端接受：`accept=false` 结束；否则从 `resume_offset` seek 续传。
    ///
    /// 返回需要立即发出的附加帧：只有「接收端已落盘长度 == 文件大小」时会返回
    /// 一个空的 `last=true` 块（让接收端做最终校验并回 `FileResult`）。
    pub fn on_accept(&mut self, accept: &FileAccept) -> Result<Vec<Envelope>, FileError> {
        let (offer, path) = match &self.state {
            SenderState::AwaitAccept { offer, path } => (offer.clone(), path.clone()),
            _ => return Err(FileError::BadState("当前没有待确认的 FileOffer")),
        };
        if accept.transfer_id != offer.transfer_id {
            return Err(FileError::TransferMismatch {
                expected: offer.transfer_id,
                got: accept.transfer_id,
            });
        }
        if !accept.accept {
            self.state = SenderState::Rejected {
                transfer_id: offer.transfer_id,
            };
            return Ok(Vec::new());
        }
        if offer.is_dir {
            // 目录 offer 只负责建目录，没有数据与结果。
            self.state = SenderState::Finished {
                transfer_id: offer.transfer_id,
                ok: true,
            };
            return Ok(Vec::new());
        }
        if accept.resume_offset > offer.size {
            return Err(FileError::ResumeMismatch {
                expected: offer.size,
                got: accept.resume_offset,
            });
        }

        let (file, hasher) = open_and_hash_prefix(&path, offer.size, accept.resume_offset)?;
        self.stats = FileTransferStats {
            resumed_from: accept.resume_offset,
            ..FileTransferStats::default()
        };
        let mut active = Box::new(ActiveSend {
            transfer_id: offer.transfer_id,
            path,
            size: offer.size,
            file,
            sent: accept.resume_offset,
            acked: accept.resume_offset,
            hasher,
            last_emitted: false,
        });

        if accept.resume_offset == offer.size {
            // 接收端已经拥有完整长度：直接空 last 块收尾（内容一致性由 BLAKE3 最终校验）。
            active.last_emitted = true;
            self.stats.digest = Some(*active.hasher.finalize().as_bytes());
            self.stats.chunks_sent = 1;
            self.state = SenderState::Sending(active);
            return Ok(vec![Envelope::file_chunk(
                offer.transfer_id,
                offer.size,
                Vec::new(),
                true,
            )]);
        }

        self.state = SenderState::Sending(active);
        Ok(Vec::new())
    }

    /// 产出下一块数据；`None` 表示「暂时没有可发的」：
    ///
    /// - 已发完 `last` 块（等 `FileResult`）；
    /// - 在途未确认字节达到窗口上限（等 `FileAck` 解锁，见 [`Self::is_blocked_on_ack`]）；
    /// - 当前状态还没有进入 `Sending`。
    ///
    /// `max_bytes == 0` 属于调用错误，返回 [`FileError::Config`]。
    pub fn next_chunk(&mut self, max_bytes: usize) -> Result<Option<Envelope>, FileError> {
        if max_bytes == 0 {
            return Err(FileError::Config("next_chunk(max_bytes) 必须 > 0"));
        }
        let effective = max_bytes.min(self.config.max_chunk_bytes.max(1));
        let SenderState::Sending(active) = &mut self.state else {
            return Ok(None);
        };
        if active.last_emitted {
            return Ok(None);
        }
        let in_flight = active.sent.saturating_sub(active.acked);
        if in_flight >= self.config.in_flight_bytes {
            return Ok(None);
        }
        let window = self.config.in_flight_bytes - in_flight;
        let remaining = active.size - active.sent;
        let want = if remaining == 0 {
            // 空文件（size=0）或恰好发完：发一个空 last 块。
            0
        } else {
            effective.min(window as usize).min(remaining as usize)
        };

        let mut buf = vec![0u8; want];
        if want > 0 {
            active
                .file
                .read_exact(&mut buf)
                .map_err(|err| FileError::io(&active.path, err))?;
        }
        let offset = active.sent;
        active.hasher.update(&buf);
        active.sent += want as u64;
        let last = active.sent == active.size;
        if last {
            active.last_emitted = true;
            self.stats.digest = Some(*active.hasher.finalize().as_bytes());
        }
        let transfer_id = active.transfer_id;
        self.stats.bytes_sent += want as u64;
        self.stats.chunks_sent += 1;
        Ok(Some(Envelope::file_chunk(transfer_id, offset, buf, last)))
    }

    /// 处理对端流控确认（只前进不后退；过期/未知 ID 忽略）。
    pub fn on_ack(&mut self, ack: &FileAck) {
        let SenderState::Sending(active) = &mut self.state else {
            tracing::debug!(
                transfer_id = ack.transfer_id,
                "收到 Ack 但当前不在发送态, 忽略"
            );
            return;
        };
        if ack.transfer_id != active.transfer_id {
            tracing::debug!(
                expected = active.transfer_id,
                got = ack.transfer_id,
                "Ack 传输 ID 不匹配, 忽略"
            );
            return;
        }
        if ack.received > active.sent {
            tracing::warn!(
                transfer_id = ack.transfer_id,
                received = ack.received,
                sent = active.sent,
                "Ack 超过已发送字节, 按已发送字节截断"
            );
        }
        active.acked = active.acked.max(ack.received.min(active.sent));
    }

    /// 处理对端最终结果：`ok=false` 返回 [`FileError::TransferFailed`]；
    /// 摘要不一致返回 [`FileError::DigestMismatch`]（内容损坏）。
    pub fn on_result(&mut self, result: &FileResult) -> Result<(), FileError> {
        let expected_id = match &self.state {
            SenderState::Sending(active) => active.transfer_id,
            SenderState::Finished { transfer_id, .. } => *transfer_id,
            _ => return Err(FileError::BadState("当前没有等待结果的传输")),
        };
        if result.transfer_id != expected_id {
            return Err(FileError::TransferMismatch {
                expected: expected_id,
                got: result.transfer_id,
            });
        }
        if !result.ok {
            self.state = SenderState::Finished {
                transfer_id: expected_id,
                ok: false,
            };
            return Err(FileError::TransferFailed(result.error.clone()));
        }
        if let Some(local) = self.stats.digest {
            if !result.digest.is_empty() && result.digest.as_slice() != local.as_slice() {
                self.state = SenderState::Finished {
                    transfer_id: expected_id,
                    ok: false,
                };
                return Err(FileError::DigestMismatch {
                    local: hex::encode(local),
                    remote: hex::encode(&result.digest),
                });
            }
        }
        self.state = SenderState::Finished {
            transfer_id: expected_id,
            ok: true,
        };
        Ok(())
    }

    /// 本次（或最近一次）传输的统计。
    pub fn stats(&self) -> FileTransferStats {
        self.stats.clone()
    }

    /// 是否已完成当前传输（收到 `FileResult` 或目录 offer 被接受）。
    pub fn is_complete(&self) -> bool {
        matches!(&self.state, SenderState::Finished { .. })
    }

    /// 是否被对端拒绝（`accept=false`）。
    pub fn is_rejected(&self) -> bool {
        matches!(&self.state, SenderState::Rejected { .. })
    }

    /// 是否以失败收尾（对端回 `ok=false`，或本地摘要校验不通过）。
    pub fn is_failed(&self) -> bool {
        matches!(&self.state, SenderState::Finished { ok: false, .. })
    }

    /// 是否因在途窗口满而不宜继续发（等 `FileAck`）。
    pub fn is_blocked_on_ack(&self) -> bool {
        match &self.state {
            SenderState::Sending(active) => {
                !active.last_emitted
                    && active.sent.saturating_sub(active.acked) >= self.config.in_flight_bytes
            }
            _ => false,
        }
    }

    /// 在途未确认字节数。
    pub fn in_flight_bytes(&self) -> u64 {
        match &self.state {
            SenderState::Sending(active) => active.sent.saturating_sub(active.acked),
            _ => 0,
        }
    }

    /// 当前/最近一次传输 ID。
    pub fn transfer_id(&self) -> Option<u64> {
        match &self.state {
            SenderState::AwaitAccept { offer, .. } => Some(offer.transfer_id),
            SenderState::Sending(active) => Some(active.transfer_id),
            SenderState::Finished { transfer_id, .. } | SenderState::Rejected { transfer_id } => {
                Some(*transfer_id)
            }
            SenderState::Idle => None,
        }
    }

    /// 目录展开后还有多少条目未 offer。
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// 放弃未发出的目录条目（不影响当前已经在途的传输）。
    pub fn clear_pending(&mut self) {
        self.pending.clear();
    }

    /// 内部：分配单调递增的 transfer_id。
    fn alloc_transfer_id(&mut self) -> u64 {
        let id = self.next_transfer_id;
        self.next_transfer_id = self.next_transfer_id.wrapping_add(1);
        id
    }

    /// 内部：当前是否允许开始新传输。
    fn can_start(&self) -> bool {
        matches!(
            &self.state,
            SenderState::Idle | SenderState::Finished { .. } | SenderState::Rejected { .. }
        )
    }
}

/// 从本机路径取文件名并净化（不上送绝对路径）。
fn path_to_offer_name(path: &Path) -> Result<String, FileError> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| FileError::invalid(format!("路径没有可传输的文件名: {}", path.display())))?;
    crate::sanitize_file_name(name)
}

/// 打开文件、校验大小，并在需要续传时流式补算前缀 BLAKE3，最后 seek 到续传点。
///
/// 前缀补算让 `stats.digest` 始终覆盖**整个文件**（而不是仅本次续传部分），
/// 内存占用恒定为一个块大小。
fn open_and_hash_prefix(
    path: &Path,
    size: u64,
    resume: u64,
) -> Result<(File, blake3::Hasher), FileError> {
    let mut file = File::open(path).map_err(|err| FileError::io(path, err))?;
    let actual = file
        .metadata()
        .map_err(|err| FileError::io(path, err))?
        .len();
    if actual != size {
        return Err(FileError::SizeMismatch {
            declared: size,
            actual,
        });
    }
    let mut hasher = blake3::Hasher::new();
    if resume > 0 {
        let mut buf = vec![0u8; DEFAULT_CHUNK_BYTES];
        let mut left = resume;
        while left > 0 {
            let want = left.min(buf.len() as u64) as usize;
            file.read_exact(&mut buf[..want])
                .map_err(|err| FileError::io(path, err))?;
            hasher.update(&buf[..want]);
            left -= want as u64;
        }
    }
    // 已读到 resume：文件游标正好在续传点；显式 seek 让意图更清晰。
    file.seek(SeekFrom::Start(resume))
        .map_err(|err| FileError::io(path, err))?;
    Ok((file, hasher))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use dfy_proto::envelope;
    use tempfile::tempdir;

    use super::*;

    fn write_file(path: &Path, len: usize) {
        let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        fs::write(path, data).unwrap();
    }

    #[test]
    fn offer_uses_file_name_only_and_metadata() {
        let tmp = tempdir().unwrap();
        let file = tmp.path().join("报告 2026.bin");
        write_file(&file, 1234);
        let mut sender = FileSender::new(10);
        let offer = sender.offer(&file).unwrap();
        assert_eq!(offer.name, "报告 2026.bin");
        assert_eq!(offer.size, 1234);
        assert!(!offer.is_dir);
        assert_eq!(offer.transfer_id, 10);
        assert!(offer.last_modified_ms > 0);
    }

    #[test]
    fn directory_offer_expands_plan_and_empty_dirs() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("photos");
        fs::create_dir_all(dir.join("empty")).unwrap();
        fs::write(dir.join("a.jpg"), b"aaa").unwrap();
        let mut sender = FileSender::new(1);
        let offer = sender.offer(&dir).unwrap();
        assert!(offer.is_dir);
        assert_eq!(offer.name, "photos");
        assert_eq!(offer.size, 0);
        assert_eq!(sender.pending_len(), 2); // empty + a.jpg

        let accept = FileAccept {
            transfer_id: offer.transfer_id,
            resume_offset: 0,
            accept: true,
        };
        assert!(sender.on_accept(&accept).unwrap().is_empty());
        assert!(sender.is_complete());

        // 展开清单按相对路径字节序（`a.jpg` < `empty`），文件先于空目录。
        let file_offer = sender.next_offer().unwrap().unwrap();
        assert_eq!(file_offer.name, "photos/a.jpg");
        assert!(!file_offer.is_dir);
        assert_eq!(file_offer.size, 3);
        let file_accept = FileAccept {
            transfer_id: file_offer.transfer_id,
            resume_offset: 0,
            accept: true,
        };
        sender.on_accept(&file_accept).unwrap();
        let chunk = sender.next_chunk(256 * 1024).unwrap().unwrap();
        assert!(matches!(
            chunk.body,
            Some(dfy_proto::envelope::Body::FileChunk(_))
        ));
        assert!(sender.next_chunk(256 * 1024).unwrap().is_none());
        let result = dfy_proto::FileResult {
            transfer_id: file_offer.transfer_id,
            ok: true,
            error: String::new(),
            digest: sender.stats().digest.unwrap().to_vec(),
        };
        sender.on_result(&result).unwrap();
        assert!(sender.is_complete());

        let dir_offer = sender.next_offer().unwrap().unwrap();
        assert_eq!(dir_offer.name, "photos/empty");
        assert!(dir_offer.is_dir);
        let dir_accept = FileAccept {
            transfer_id: dir_offer.transfer_id,
            resume_offset: 0,
            accept: true,
        };
        sender.on_accept(&dir_accept).unwrap();

        assert!(sender.next_offer().unwrap().is_none());
    }

    #[test]
    fn in_flight_window_blocks_until_ack() {
        let tmp = tempdir().unwrap();
        let file = tmp.path().join("big.bin");
        write_file(&file, 600 * 1024);
        let mut sender = FileSender::with_config(
            1,
            SenderConfig {
                max_chunk_bytes: 256 * 1024,
                in_flight_bytes: 512 * 1024,
            },
        );
        let offer = sender.offer(&file).unwrap();
        let accept = FileAccept {
            transfer_id: offer.transfer_id,
            resume_offset: 0,
            accept: true,
        };
        sender.on_accept(&accept).unwrap();

        // 前两块：256 KiB × 2 = 512 KiB，填满窗口。
        assert!(sender.next_chunk(256 * 1024).unwrap().is_some());
        assert!(sender.next_chunk(256 * 1024).unwrap().is_some());
        assert!(sender.next_chunk(256 * 1024).unwrap().is_none());
        assert!(sender.is_blocked_on_ack());
        assert_eq!(sender.in_flight_bytes(), 512 * 1024);

        // ack 解锁后再发。
        let ack = FileAck {
            transfer_id: offer.transfer_id,
            received: 512 * 1024,
        };
        sender.on_ack(&ack);
        assert!(!sender.is_blocked_on_ack());
        assert!(sender.next_chunk(256 * 1024).unwrap().is_some());
    }

    #[test]
    fn resume_offset_seeks_and_skips_prefix() {
        let tmp = tempdir().unwrap();
        let file = tmp.path().join("resume.bin");
        write_file(&file, 1024 * 1024);
        let mut sender = FileSender::new(7);
        let offer = sender.offer(&file).unwrap();
        let accept = FileAccept {
            transfer_id: offer.transfer_id,
            resume_offset: 512 * 1024,
            accept: true,
        };
        sender.on_accept(&accept).unwrap();
        assert_eq!(sender.stats().resumed_from, 512 * 1024);
        let chunk = sender.next_chunk(256 * 1024).unwrap().unwrap();
        match chunk.body {
            Some(envelope::Body::FileChunk(chunk)) => {
                assert_eq!(chunk.offset, 512 * 1024);
                assert_eq!(chunk.data.len(), 256 * 1024);
                let expected: Vec<u8> = (512 * 1024..768 * 1024).map(|i| (i % 251) as u8).collect();
                assert_eq!(chunk.data, expected);
                assert!(!chunk.last);
            }
            other => panic!("期望 FileChunk, 实际 {other:?}"),
        }
    }

    #[test]
    fn resume_beyond_size_is_rejected() {
        let tmp = tempdir().unwrap();
        let file = tmp.path().join("small.bin");
        write_file(&file, 16);
        let mut sender = FileSender::new(1);
        let offer = sender.offer(&file).unwrap();
        let accept = FileAccept {
            transfer_id: offer.transfer_id,
            resume_offset: 17,
            accept: true,
        };
        let err = sender.on_accept(&accept).unwrap_err();
        assert!(matches!(err, FileError::ResumeMismatch { .. }));
    }

    #[test]
    fn reject_accept_sets_rejected_state() {
        let tmp = tempdir().unwrap();
        let file = tmp.path().join("x.bin");
        write_file(&file, 8);
        let mut sender = FileSender::new(1);
        let offer = sender.offer(&file).unwrap();
        let accept = FileAccept {
            transfer_id: offer.transfer_id,
            resume_offset: 0,
            accept: false,
        };
        assert!(sender.on_accept(&accept).unwrap().is_empty());
        assert!(sender.is_rejected());
        assert!(!sender.is_complete());
    }
}
