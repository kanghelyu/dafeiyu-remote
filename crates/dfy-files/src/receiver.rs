// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 接收端状态机：把 [`FileChunk`] 落盘到接收根内，按需回 `FileAck` / `FileResult`。
//!
//! 关键语义（对应 `spec/DFYP-v1.md` §9）：
//!
//! 1. **先写 `.dfy-part`，校验通过后 rename**：中途崩溃/断线不会留下看似完整的坏文件，
//!    残留的 part 文件正好是下一次续传的已有数据；
//! 2. **续传以接收端已落盘长度为准**：`FileAccept.resume_offset` 来自本地 part 长度
//!    （或 Overwrite 策略下旧目标文件长度），**不信任**发送端声明；
//! 3. **路径净化 + 接收根约束**：`FileOffer.name` 是不可信输入，先经
//!    [`sanitize_relative_path`] 再经 `resolve_within`（逐级拒绝符号链接），
//!    绝对路径 / `..` / 盘符 / Windows 保留名一律拒绝；
//! 4. **流控**：每 16 MiB 或 64 块回一次 `FileAck`（可配置），发送端据此滑窗；
//! 5. **幂等重发**：完全落在已接收区间内的重复块静默丢弃并返回空，不推进状态。

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use dfy_proto::{Envelope, FileAccept, FileChunk, FileOffer};

use crate::path::{resolve_within, sanitize_relative_path};
use crate::{
    part_path, FileError, DEFAULT_ACK_BYTES, DEFAULT_ACK_CHUNKS, DEFAULT_CHUNK_BYTES, DIGEST_BYTES,
};

/// 目标已存在时的处理策略。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OverwritePolicy {
    /// 覆盖：目标已存在时按「已有长度」续传（旧内容当前缀，最终由 BLAKE3 兜住正确性），
    /// 校验通过后替换目标。
    Overwrite,
    /// 跳过：目标已存在时回 `FileAccept{accept:false}`，不消耗带宽。
    #[default]
    Skip,
    /// 重命名：目标已存在时自动选 `name (1).ext` 之类的空闲名。
    Rename,
}

/// 接收端配置。
#[derive(Clone, Copy, Debug)]
pub struct ReceiveConfig {
    /// 每收到这么多字节回一次 `FileAck`（默认 16 MiB）。
    pub ack_bytes: u64,
    /// 每收到这么多块回一次 `FileAck`（默认 64 块）。
    pub ack_chunks: u32,
    /// 单块防御上限（默认 256 KiB）：超过视为对端实现错误/恶意，直接报错。
    pub max_chunk_bytes: usize,
    /// 单文件声明大小上限（默认 `u64::MAX`，即不限制）。
    ///
    /// 网络对端不可信：CLI/会话层应按本地策略设置一个上限（例如 64 GiB），
    /// 在创建临时文件前就以 [`FileError::FileTooLarge`] 拒掉超大的 offer，
    /// 避免恶意/错误的对端用巨大 `size` 打满磁盘。0 表示拒绝一切非空文件。
    pub max_file_bytes: u64,
}

impl Default for ReceiveConfig {
    fn default() -> Self {
        Self {
            ack_bytes: DEFAULT_ACK_BYTES,
            ack_chunks: DEFAULT_ACK_CHUNKS,
            max_chunk_bytes: DEFAULT_CHUNK_BYTES,
            max_file_bytes: u64::MAX,
        }
    }
}

/// 接收端统计。
///
/// 每次 `on_offer` 重置每传输字段；`files_completed` / `dirs_created` / `bytes_total`
/// 在整个接收器生命周期内累计（便于 UI 显示「本会话共收了多少」）。
#[derive(Clone, Debug, Default)]
pub struct FileReceiveStats {
    /// 本次传输接收的文件负载字节（不含续传前缀）。
    pub bytes_received: u64,
    /// 本次传输实际写入的有效块数（重复块不计）。
    pub chunks_received: u64,
    /// 已通过 `FileAck` 向发送端确认的最大绝对偏移。
    pub acked_bytes: u64,
    /// 续传起点（已有 part / 旧目标长度，0=从头收）。
    pub resumed_from: u64,
    /// 最近一次完成的文件 BLAKE3。
    pub digest: Option<[u8; DIGEST_BYTES]>,
    /// 本接收器累计完成的文件数。
    pub files_completed: u64,
    /// 本接收器累计创建的目录数（含 offer 的目录项）。
    pub dirs_created: u64,
    /// 本接收器累计写入字节数（跨传输）。
    pub bytes_total: u64,
}

/// 正在接收的文件。
struct ActiveReceive {
    transfer_id: u64,
    /// 目标最终路径（已过 sanitize + resolve_within + 策略处理）。
    target: PathBuf,
    /// 临时落盘路径 `<target>.dfy-part`。
    part: PathBuf,
    /// 临时文件句柄；收尾时 `take` 出来以便 rename 前关闭。
    file: Option<File>,
    /// offer 声明大小。
    size: u64,
    /// 下一个期望的绝对偏移（= 已落盘长度）。
    expected: u64,
    /// 流式 BLAKE3（续传前缀已在 `on_offer` 补算）。
    hasher: blake3::Hasher,
    /// 已确认偏移（用于统计 `acked_bytes`）。
    acked: u64,
    /// 自上次 ack 以来未确认字节/块数。
    unacked_bytes: u64,
    unacked_chunks: u32,
    /// 已回过 `FileResult`（防止重复收尾）。
    last_reported: bool,
}

/// 收尾时需要跨借用作用域携带的信息（此时临时文件句柄已取出，可安全 rename）。
struct FinishInfo {
    transfer_id: u64,
    target: PathBuf,
    part: PathBuf,
    expected: u64,
    declared: u64,
    digest: [u8; DIGEST_BYTES],
    file: File,
}

/// 接收端状态。
enum ReceiverState {
    /// 空闲，可接受新的 `FileOffer`。
    Idle,
    /// 正在接收一个文件；`Box` 避免 BLAKE3 状态体撑大 enum。
    Receiving(Box<ActiveReceive>),
    /// 上一个文件已收尾（成功/失败），等待下一个 offer。
    Done { transfer_id: u64 },
}

/// 接收端：与网络解耦的文件接收状态机。
///
/// 目标根 `root` 是安全边界：所有写入路径都必须解析到这个目录**之内**。
/// 建议把 root 设为专用目录（例如 `%USERPROFILE%\Downloads\大肥鱼接收`）。
pub struct FileReceiver {
    root: PathBuf,
    policy: OverwritePolicy,
    config: ReceiveConfig,
    state: ReceiverState,
    stats: FileReceiveStats,
}

impl FileReceiver {
    /// 用接收根与覆盖策略构造（默认 ack 间隔 16 MiB / 64 块）。
    pub fn new(root: PathBuf, policy: OverwritePolicy) -> Self {
        Self::with_config(root, policy, ReceiveConfig::default())
    }

    /// 带配置构造。
    pub fn with_config(root: PathBuf, policy: OverwritePolicy, config: ReceiveConfig) -> Self {
        Self {
            root,
            policy,
            config,
            state: ReceiverState::Idle,
            stats: FileReceiveStats::default(),
        }
    }

    /// 处理 `FileOffer`：净化路径 → 应用策略 → 决定续传起点 → 回 `FileAccept`。
    ///
    /// 返回 `(FileAccept, 目标路径)`；`accept=false`（Skip 命中已存在文件）同样返回目标路径，
    /// 便于 UI 提示「已跳过」。
    ///
    /// 目录 offer（`is_dir=true`）只创建目录并直接接受，不进入接收态。
    pub fn on_offer(&mut self, offer: &FileOffer) -> Result<(FileAccept, PathBuf), FileError> {
        if matches!(&self.state, ReceiverState::Receiving(_)) {
            return Err(FileError::BadState(
                "上一个文件尚未接收完毕，不能开始新 offer",
            ));
        }
        // 每传输字段重置，累计字段保留。
        let previous = self.stats.clone();
        self.stats = FileReceiveStats {
            files_completed: previous.files_completed,
            dirs_created: previous.dirs_created,
            bytes_total: previous.bytes_total,
            ..FileReceiveStats::default()
        };

        let relative = sanitize_relative_path(&offer.name)?;
        if !offer.is_dir && offer.size > self.config.max_file_bytes {
            return Err(FileError::FileTooLarge {
                declared: offer.size,
                max: self.config.max_file_bytes,
            });
        }
        let mut target = resolve_within(&self.root, &relative)?;

        if offer.is_dir {
            match fs::symlink_metadata(&target) {
                Ok(md) if !md.is_dir() => {
                    return Err(FileError::TargetTypeMismatch {
                        path: target.display().to_string(),
                    })
                }
                Ok(_) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(FileError::io(&target, err)),
            }
            fs::create_dir_all(&target).map_err(|err| FileError::io(&target, err))?;
            self.stats.dirs_created += 1;
            self.state = ReceiverState::Done {
                transfer_id: offer.transfer_id,
            };
            return Ok((
                FileAccept {
                    transfer_id: offer.transfer_id,
                    resume_offset: 0,
                    accept: true,
                },
                target,
            ));
        }

        // 目标已存在：按策略决定跳过 / 改名 / 续传。
        let mut existed = false;
        match fs::symlink_metadata(&target) {
            Ok(md) if md.is_dir() => {
                return Err(FileError::TargetTypeMismatch {
                    path: target.display().to_string(),
                })
            }
            Ok(md) => {
                existed = true;
                if self.policy == OverwritePolicy::Skip {
                    self.state = ReceiverState::Done {
                        transfer_id: offer.transfer_id,
                    };
                    tracing::debug!(path = %target.display(), "目标已存在, 策略 Skip 回绝");
                    return Ok((
                        FileAccept {
                            transfer_id: offer.transfer_id,
                            resume_offset: 0,
                            accept: false,
                        },
                        target,
                    ));
                }
                if self.policy == OverwritePolicy::Rename {
                    target = unique_target_path(&target)?;
                    tracing::debug!(path = %target.display(), "目标已存在, 策略 Rename 改名");
                    existed = false;
                } else {
                    // Overwrite 且目标是普通文件：把它当前缀续传（大小已在 seed_part 里核对）。
                    tracing::debug!(len = md.len(), path = %target.display(), "目标已存在, 按已有长度续传");
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(FileError::io(&target, err)),
        }

        // 父目录必须在 resolve_within 之后创建（它已逐级检查过符号链接）。
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|err| FileError::io(parent, err))?;
        }

        let part = part_path(&target);
        let (file, resume, hasher) = seed_part(&part, &target, offer.size, self.policy, existed)?;
        self.stats.resumed_from = resume;
        self.state = ReceiverState::Receiving(Box::new(ActiveReceive {
            transfer_id: offer.transfer_id,
            target: target.clone(),
            part,
            file: Some(file),
            size: offer.size,
            expected: resume,
            hasher,
            acked: resume,
            unacked_bytes: 0,
            unacked_chunks: 0,
            last_reported: false,
        }));
        Ok((
            FileAccept {
                transfer_id: offer.transfer_id,
                resume_offset: resume,
                accept: true,
            },
            target,
        ))
    }

    /// 处理 `FileChunk`：校验偏移 → 写入 part → 按需产出 `FileAck`；
    /// `last=true` 时 flush + 校验大小 + BLAKE3 → rename 落盘 → 产出 `FileResult`。
    ///
    /// 返回需要发回的 [`Envelope`] 列表（可能为空）：
    ///
    /// - 普通块未到 ack 阈值：空；
    /// - 到达 16 MiB / 64 块：一条 `FileAck`；
    /// - 完全重复的块（offset+len ≤ 已收长度）：空（幂等丢弃）；
    /// - 最后一块：`FileAck` + `FileResult`（大小不符时为 `ok=false`，part 保留供续传）。
    pub fn on_chunk(&mut self, chunk: &FileChunk) -> Result<Vec<Envelope>, FileError> {
        let mut out: Vec<Envelope> = Vec::new();
        let finish = {
            let Self {
                state,
                stats,
                config,
                ..
            } = self;
            let ReceiverState::Receiving(active) = state else {
                return Err(FileError::BadState("当前没有正在接收的文件"));
            };
            if chunk.transfer_id != active.transfer_id {
                return Err(FileError::TransferMismatch {
                    expected: active.transfer_id,
                    got: chunk.transfer_id,
                });
            }
            if chunk.data.len() > config.max_chunk_bytes {
                return Err(FileError::ChunkTooLarge {
                    len: chunk.data.len(),
                    max: config.max_chunk_bytes,
                });
            }
            if active.last_reported {
                return Err(FileError::BadState("传输已收尾，仍收到数据块"));
            }

            let len = chunk.data.len() as u64;
            if chunk.offset != active.expected {
                let end = chunk.offset.saturating_add(len);
                if chunk.offset < active.expected && end <= active.expected {
                    // 完全重复（典型原因：FileAck 丢失后发送端重发）。幂等丢弃，不推进状态。
                    tracing::debug!(
                        transfer_id = chunk.transfer_id,
                        offset = chunk.offset,
                        expected = active.expected,
                        "忽略重复数据块"
                    );
                    return Ok(Vec::new());
                }
                return Err(FileError::ChunkOutOfOrder {
                    expected: active.expected,
                    got: chunk.offset,
                });
            }
            let new_expected = active.expected.saturating_add(len);
            if new_expected > active.size {
                return Err(FileError::SizeMismatch {
                    declared: active.size,
                    actual: new_expected,
                });
            }

            let file = active
                .file
                .as_mut()
                .ok_or(FileError::BadState("临时文件句柄已关闭"))?;
            file.write_all(&chunk.data)
                .map_err(|err| FileError::io(&active.part, err))?;
            active.hasher.update(&chunk.data);
            active.expected = new_expected;
            active.unacked_bytes += len;
            active.unacked_chunks += 1;
            stats.bytes_received += len;
            stats.chunks_received += 1;

            if chunk.last {
                file.flush()
                    .map_err(|err| FileError::io(&active.part, err))?;
                // 最后一块后 sync_all：保证 rename 前数据真正落盘（崩溃也不丢已确认数据）。
                file.sync_all()
                    .map_err(|err| FileError::io(&active.part, err))?;
                let digest = *active.hasher.finalize().as_bytes();
                active.last_reported = true;
                Some(FinishInfo {
                    transfer_id: active.transfer_id,
                    target: active.target.clone(),
                    part: active.part.clone(),
                    expected: active.expected,
                    declared: active.size,
                    digest,
                    // 取出句柄：drop 之后才 rename（Windows 上更稳）。
                    file: active
                        .file
                        .take()
                        .ok_or(FileError::BadState("临时文件句柄已关闭"))?,
                })
            } else {
                let need_ack = active.unacked_bytes >= config.ack_bytes
                    || active.unacked_chunks >= config.ack_chunks;
                if !need_ack {
                    return Ok(Vec::new());
                }
                active.unacked_bytes = 0;
                active.unacked_chunks = 0;
                active.acked = active.expected;
                stats.acked_bytes = active.expected;
                out.push(Envelope::file_ack(active.transfer_id, active.expected));
                None
            }
        };

        let Some(finish) = finish else {
            return Ok(out);
        };
        // 关闭临时文件句柄，再 rename。
        drop(finish.file);

        if finish.expected != finish.declared {
            // 大小不符：不落盘，保留 part 供下一次续传；如实回 ok=false。
            self.state = ReceiverState::Done {
                transfer_id: finish.transfer_id,
            };
            let msg = format!(
                "大小不符: 声明 {} 字节, 实际收到 {} 字节",
                finish.declared, finish.expected
            );
            return Ok(vec![Envelope::file_result(
                finish.transfer_id,
                false,
                msg,
                Vec::new(),
            )]);
        }

        if let Err(err) = replace_file(&finish.part, &finish.target) {
            self.state = ReceiverState::Done {
                transfer_id: finish.transfer_id,
            };
            return Err(FileError::io(&finish.target, err));
        }

        self.stats.digest = Some(finish.digest);
        self.stats.files_completed += 1;
        self.stats.bytes_total += finish.expected;
        self.stats.acked_bytes = finish.expected;
        self.state = ReceiverState::Done {
            transfer_id: finish.transfer_id,
        };
        out.push(Envelope::file_ack(finish.transfer_id, finish.expected));
        out.push(Envelope::file_result(
            finish.transfer_id,
            true,
            String::new(),
            finish.digest.to_vec(),
        ));
        Ok(out)
    }

    /// 接收端统计。
    pub fn stats(&self) -> FileReceiveStats {
        self.stats.clone()
    }

    /// 接收根。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 当前覆盖策略。
    pub fn policy(&self) -> OverwritePolicy {
        self.policy
    }

    /// 是否正在接收某个文件。
    pub fn is_receiving(&self) -> bool {
        matches!(&self.state, ReceiverState::Receiving(_))
    }

    /// 当前/最近一次传输 ID。
    pub fn transfer_id(&self) -> Option<u64> {
        match &self.state {
            ReceiverState::Idle => None,
            ReceiverState::Receiving(active) => Some(active.transfer_id),
            ReceiverState::Done { transfer_id } => Some(*transfer_id),
        }
    }

    /// 中止当前接收：关闭临时文件、回到空闲（part 保留，下次可续传）。
    pub fn abort(&mut self) {
        if let ReceiverState::Receiving(active) = &self.state {
            tracing::debug!(
                transfer_id = active.transfer_id,
                path = %active.part.display(),
                "中止接收, 保留 part 供续传"
            );
        }
        self.state = ReceiverState::Idle;
    }
}

/// 准备临时 part 文件并决定续传起点。
///
/// 优先级：
/// 1. 已有 `.dfy-part`（上次中断）→ 以其长度为续传起点；比声明大小还长说明被污染，截断重来；
/// 2. Overwrite 策略且目标已存在普通文件 → 把旧文件内容复制进 part 当前缀（「接收端以已落盘
///    长度为续传起点」；最终 BLAKE3 校验会兜住「旧内容不是新文件前缀」的情况）；
/// 3. 否则从空文件开始。
///
/// part 内容会流式补算 BLAKE3，最后 seek 到续传点，保证摘要覆盖整个文件。
fn seed_part(
    part: &Path,
    target: &Path,
    size: u64,
    policy: OverwritePolicy,
    target_existed: bool,
) -> Result<(File, u64, blake3::Hasher), FileError> {
    let mut resume: u64 = 0;
    let mut reuse_existing = false;
    match fs::symlink_metadata(part) {
        Ok(md) if md.is_file() => {
            if md.len() > 0 && md.len() <= size {
                resume = md.len();
                reuse_existing = true;
            }
            // part 比声明大小还长：视为污点，下面截断重来。
        }
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(FileError::io(part, err)),
    }

    if !reuse_existing && resume == 0 && policy == OverwritePolicy::Overwrite && target_existed {
        if let Ok(md) = fs::symlink_metadata(target) {
            if md.is_file() && md.len() > 0 && md.len() <= size {
                // 旧目标文件 → part 前缀。用 copy 而非整读入内存。
                let mut src = File::open(target).map_err(|err| FileError::io(target, err))?;
                let mut dst = OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(part)
                    .map_err(|err| FileError::io(part, err))?;
                io::copy(&mut src, &mut dst).map_err(|err| FileError::io(part, err))?;
                dst.sync_all().map_err(|err| FileError::io(part, err))?;
                resume = md.len();
                reuse_existing = true;
            }
        }
    }

    let mut file = if reuse_existing {
        // 必须同时有读权限：下面要回读已有前缀补算 BLAKE3（Windows 只给写权限会拒绝读）。
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .open(part)
            .map_err(|err| FileError::io(part, err))?;
        f.set_len(resume).map_err(|err| FileError::io(part, err))?;
        f
    } else {
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(part)
            .map_err(|err| FileError::io(part, err))?
    };

    // 流式补算已有前缀的 BLAKE3。
    let mut hasher = blake3::Hasher::new();
    if resume > 0 {
        let mut buf = vec![0u8; DEFAULT_CHUNK_BYTES];
        let mut left = resume;
        while left > 0 {
            let want = left.min(buf.len() as u64) as usize;
            file.read_exact(&mut buf[..want])
                .map_err(|err| FileError::io(part, err))?;
            hasher.update(&buf[..want]);
            left -= want as u64;
        }
    }
    file.seek(SeekFrom::Start(resume))
        .map_err(|err| FileError::io(part, err))?;
    Ok((file, resume, hasher))
}

/// 把 part 文件落到最终目标（尽量原子）。
///
/// Unix：`rename` 直接原子替换；Windows：目标已存在时先删除再 rename，
/// 并兜住「删除与 rename 之间的竞争」重试一次。
fn replace_file(part: &Path, target: &Path) -> io::Result<()> {
    match fs::rename(part, target) {
        Ok(()) => return Ok(()),
        Err(err) if target.exists() => {
            tracing::debug!(target = %target.display(), error = %err, "rename 失败且目标存在, 删除后重试");
        }
        Err(err) => return Err(err),
    }
    if let Ok(md) = fs::symlink_metadata(target) {
        if md.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "目标路径是目录, 拒绝覆盖",
            ));
        }
        fs::remove_file(target)?;
    }
    fs::rename(part, target)
}

/// 为 Rename 策略生成空闲目标名：`name.ext` → `name (1).ext` → `name (2).ext` …
fn unique_target_path(target: &Path) -> Result<PathBuf, FileError> {
    const MAX_ATTEMPTS: usize = 1000;
    let stem = target
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| FileError::invalid(format!("目标没有可用的文件名: {}", target.display())))?;
    let ext = target.extension().and_then(|s| s.to_str());
    let parent = target.parent().map(Path::to_path_buf);
    for i in 1..=MAX_ATTEMPTS {
        let name = match ext {
            Some(ext) => format!("{stem} ({i}).{ext}"),
            None => format!("{stem} ({i})"),
        };
        let candidate = match &parent {
            Some(parent) => parent.join(name),
            None => PathBuf::from(name),
        };
        match fs::symlink_metadata(&candidate) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(candidate),
            Ok(_) => continue,
            Err(err) => return Err(FileError::io(&candidate, err)),
        }
    }
    Err(FileError::Config(
        "Rename 策略尝试次数超限（1000 个同名文件）",
    ))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    fn offer(name: &str, size: u64) -> FileOffer {
        FileOffer {
            transfer_id: 1,
            name: name.to_string(),
            size,
            last_modified_ms: 0,
            is_dir: false,
        }
    }

    fn chunk(offset: u64, data: &[u8], last: bool) -> FileChunk {
        FileChunk {
            transfer_id: 1,
            offset,
            data: data.to_vec(),
            last,
        }
    }

    #[test]
    fn rejects_traversal_offer() {
        let tmp = tempdir().unwrap();
        let mut recv = FileReceiver::new(tmp.path().join("root"), OverwritePolicy::Skip);
        let err = recv.on_offer(&offer("../../evil", 1)).unwrap_err();
        assert!(matches!(err, FileError::InvalidPath(_)));
        let err = recv.on_offer(&offer(r"C:\Windows\evil", 1)).unwrap_err();
        assert!(matches!(err, FileError::InvalidPath(_)));
        let err = recv.on_offer(&offer("aux.txt", 1)).unwrap_err();
        assert!(matches!(err, FileError::InvalidPath(_)));
    }

    #[test]
    fn small_file_roundtrip_with_ack_threshold() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("root");
        let mut recv = FileReceiver::with_config(
            root.clone(),
            OverwritePolicy::Skip,
            ReceiveConfig {
                ack_bytes: 8,
                ack_chunks: 100,
                max_chunk_bytes: 4,
                max_file_bytes: u64::MAX,
            },
        );
        let (accept, target) = recv.on_offer(&offer("a.bin", 10)).unwrap();
        assert!(accept.accept);
        assert_eq!(accept.resume_offset, 0);
        assert_eq!(target, root.join("a.bin"));

        let out = recv.on_chunk(&chunk(0, b"0123", false)).unwrap();
        assert!(out.is_empty(), "未到 8 字节的 ack 阈值, 不应回 FileAck");
        let out = recv.on_chunk(&chunk(4, b"4567", false)).unwrap();
        assert_eq!(out.len(), 1);
        assert!(matches!(
            out[0].body,
            Some(dfy_proto::envelope::Body::FileAck(_))
        ));
        let out = recv.on_chunk(&chunk(8, b"89", true)).unwrap();
        assert_eq!(out.len(), 2);
        assert!(matches!(
            out[0].body,
            Some(dfy_proto::envelope::Body::FileAck(_))
        ));
        assert!(matches!(
            out[1].body,
            Some(dfy_proto::envelope::Body::FileResult(_))
        ));
        assert_eq!(fs::read(&target).unwrap(), b"0123456789");
        assert_eq!(recv.stats().files_completed, 1);
        // part 已 rename，不残留。
        assert!(!root.join("a.bin.dfy-part").exists());
    }

    #[test]
    fn duplicate_chunk_is_dropped_and_gap_is_error() {
        let tmp = tempdir().unwrap();
        let mut recv = FileReceiver::new(tmp.path().join("root"), OverwritePolicy::Skip);
        recv.on_offer(&offer("d.bin", 8)).unwrap();
        recv.on_chunk(&chunk(0, b"aaaa", false)).unwrap();
        // 重复块：幂等丢弃。
        let out = recv.on_chunk(&chunk(0, b"aaaa", false)).unwrap();
        assert!(out.is_empty());
        // 缺口：报 ChunkOutOfOrder 且状态保持。
        let err = recv.on_chunk(&chunk(6, b"xx", false)).unwrap_err();
        assert!(matches!(
            err,
            FileError::ChunkOutOfOrder {
                expected: 4,
                got: 6
            }
        ));
        // 正确的下一块仍可继续。
        recv.on_chunk(&chunk(4, b"bbbb", true)).unwrap();
    }

    #[test]
    fn skip_policy_rejects_existing_file() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("root");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("x.bin"), b"old").unwrap();
        let mut recv = FileReceiver::new(root.clone(), OverwritePolicy::Skip);
        let (accept, _) = recv.on_offer(&offer("x.bin", 3)).unwrap();
        assert!(!accept.accept);
        assert_eq!(fs::read(root.join("x.bin")).unwrap(), b"old");
    }

    #[test]
    fn rename_policy_picks_free_name() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("root");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("r.txt"), b"old").unwrap();
        let mut recv = FileReceiver::new(root.clone(), OverwritePolicy::Rename);
        let (accept, target) = recv.on_offer(&offer("r.txt", 3)).unwrap();
        assert!(accept.accept);
        assert_eq!(target, root.join("r (1).txt"));
        recv.on_chunk(&chunk(0, b"new", true)).unwrap();
        assert_eq!(fs::read(root.join("r (1).txt")).unwrap(), b"new");
        assert_eq!(fs::read(root.join("r.txt")).unwrap(), b"old");
    }

    #[test]
    fn overwrite_policy_resumes_from_existing_length() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("root");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("o.bin"), b"0123").unwrap();
        let mut recv = FileReceiver::new(root.clone(), OverwritePolicy::Overwrite);
        let (accept, _) = recv.on_offer(&offer("o.bin", 8)).unwrap();
        assert_eq!(accept.resume_offset, 4, "以已有目标长度续传");
        recv.on_chunk(&chunk(4, b"4567", true)).unwrap();
        assert_eq!(fs::read(root.join("o.bin")).unwrap(), b"01234567");
        assert_eq!(recv.stats().resumed_from, 4);
    }

    #[test]
    fn directory_offer_creates_dir_and_accepts() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("root");
        let mut recv = FileReceiver::new(root.clone(), OverwritePolicy::Skip);
        let mut dir_offer = offer("子目录/空目录", 0);
        dir_offer.is_dir = true;
        let (accept, target) = recv.on_offer(&dir_offer).unwrap();
        assert!(accept.accept);
        assert!(target.is_dir());
        assert!(!recv.is_receiving());
        assert_eq!(recv.stats().dirs_created, 1);
    }

    #[test]
    fn oversized_offer_is_rejected_before_creating_anything() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("root");
        let mut recv = FileReceiver::with_config(
            root.clone(),
            OverwritePolicy::Skip,
            ReceiveConfig {
                max_file_bytes: 8,
                ..ReceiveConfig::default()
            },
        );
        let err = recv.on_offer(&offer("huge.bin", 9)).unwrap_err();
        assert!(matches!(
            err,
            FileError::FileTooLarge {
                declared: 9,
                max: 8
            }
        ));
        assert!(!recv.is_receiving());
        assert!(!root.join("huge.bin.dfy-part").exists());
    }

    #[test]
    fn oversized_chunk_and_wrong_transfer_id_are_rejected() {
        let tmp = tempdir().unwrap();
        let mut recv = FileReceiver::with_config(
            tmp.path().join("root"),
            OverwritePolicy::Skip,
            ReceiveConfig {
                max_chunk_bytes: 4,
                ..ReceiveConfig::default()
            },
        );
        recv.on_offer(&offer("c.bin", 8)).unwrap();
        let too_big = FileChunk {
            transfer_id: 1,
            offset: 0,
            data: vec![0u8; 5],
            last: false,
        };
        assert!(matches!(
            recv.on_chunk(&too_big).unwrap_err(),
            FileError::ChunkTooLarge { len: 5, max: 4 }
        ));
        let wrong_id = FileChunk {
            transfer_id: 99,
            offset: 0,
            data: b"0123".to_vec(),
            last: false,
        };
        assert!(matches!(
            recv.on_chunk(&wrong_id).unwrap_err(),
            FileError::TransferMismatch {
                expected: 1,
                got: 99
            }
        ));
        // 状态保持，正确块仍可继续。
        let ok = FileChunk {
            transfer_id: 1,
            offset: 0,
            data: b"0123".to_vec(),
            last: false,
        };
        assert!(recv.on_chunk(&ok).unwrap().is_empty());
    }

    #[test]
    fn declared_size_mismatch_keeps_part_and_reports_failure() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("root");
        let mut recv = FileReceiver::new(root.clone(), OverwritePolicy::Skip);
        recv.on_offer(&offer("bad.bin", 10)).unwrap();
        let out = recv.on_chunk(&chunk(0, b"0123", true)).unwrap();
        assert_eq!(out.len(), 1);
        match &out[0].body {
            Some(dfy_proto::envelope::Body::FileResult(result)) => {
                assert!(!result.ok);
                assert!(result.digest.is_empty());
            }
            other => panic!("期望 FileResult, 实际 {other:?}"),
        }
        // part 保留供下次续传。
        assert!(root.join("bad.bin.dfy-part").exists());
        assert!(!root.join("bad.bin").exists());
    }
}
