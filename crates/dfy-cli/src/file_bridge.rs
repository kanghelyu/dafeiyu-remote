//! 把 `dfy-files` 的文件传输状态机接进 agent / view 的 control 流。
//!
//! 设计见 `design/024-file-transfer.md`：发送方 `FileOffer` → 接收方 `FileAccept`
//! → 发送方按块发 `FileChunk` → 接收方按阈值回 `FileAck` → 接收方最终回 `FileResult`。
//! v1 复用 control 流（`file-meta`/`file-data` 独立流在规范里预留但未启用）。
//!
//! 两侧状态机都不是「一问一答」：一次 `on_chunk` 可能同时产生多个 Ack，一次
//! `on_accept` 可能补一个空尾块，因此和剪贴板一样走 `outbox` 队列。

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use anyhow::Result;
use dfy_files::{FileReceiver, FileSender, OverwritePolicy, DEFAULT_CHUNK_BYTES};
use dfy_proto::envelope::Body;
use dfy_proto::Envelope;
use tracing::{debug, info, warn};

/// 每个 tick 最多发出的块数（100ms 一次 tick，8 × 256KiB ≈ 20 MiB/s 上限）。
pub const CHUNKS_PER_TICK: usize = 8;

/// 在途数据上限：超过后暂停发送，等对端 Ack（避免 control 流被单方灌满）。
pub const MAX_IN_FLIGHT_BYTES: u64 = dfy_files::DEFAULT_IN_FLIGHT_BYTES;

/// 文件传输统计（两侧共用一个结构，未用到的字段保持 0）。
#[derive(Debug, Default, Clone, Copy)]
pub struct FileStats {
    /// 发出的 offer 数。
    pub offers_sent: u64,
    /// 收到的 offer 数。
    pub offers_received: u64,
    /// 发出的 accept 数。
    pub accepts_sent: u64,
    /// 收到的 accept 数。
    pub accepts_received: u64,
    /// 发出的数据块数。
    pub chunks_sent: u64,
    /// 收到的数据块数。
    pub chunks_received: u64,
    /// 发出的 ack 数。
    pub acks_sent: u64,
    /// 收到的 ack 数。
    pub acks_received: u64,
    /// 发出的结果数。
    pub results_sent: u64,
    /// 收到的结果数。
    pub results_received: u64,
    /// 发出/接收的载荷字节数。
    pub bytes: u64,
    /// 成功完成的文件数。
    pub files_completed: u64,
    /// 失败/被拒的文件数。
    pub files_failed: u64,
}

/// 判断一条控制帧是否属于文件传输消息。
pub fn is_file_envelope(env: &Envelope) -> bool {
    matches!(
        env.body,
        Some(Body::FileOffer(_))
            | Some(Body::FileAccept(_))
            | Some(Body::FileChunk(_))
            | Some(Body::FileAck(_))
            | Some(Body::FileResult(_))
    )
}

/// 文件名（用于日志）：空名字用占位。
fn display_name(name: &str) -> &str {
    if name.trim().is_empty() {
        "<unnamed>"
    } else {
        name
    }
}

/// Agent 侧：接收文件到 `recv_dir`。
pub struct AgentFiles {
    receiver: FileReceiver,
    stats: FileStats,
}

impl AgentFiles {
    /// 创建接收状态机；`recv_dir` 不存在时自动创建。
    pub fn new(recv_dir: &Path, policy: OverwritePolicy) -> Result<Self> {
        std::fs::create_dir_all(recv_dir)
            .map_err(|err| anyhow::anyhow!("创建接收目录 {} 失败：{err}", recv_dir.display()))?;
        info!(dir = %recv_dir.display(), "文件接收已启用");
        Ok(Self {
            receiver: FileReceiver::new(recv_dir.to_path_buf(), policy),
            stats: FileStats::default(),
        })
    }

    /// 处理一条文件控制帧，返回需要回给发送方的帧。
    pub fn on_envelope(&mut self, env: &Envelope) -> Vec<Envelope> {
        let mut out = Vec::new();
        match &env.body {
            Some(Body::FileOffer(offer)) => {
                self.stats.offers_received += 1;
                match self.receiver.on_offer(offer) {
                    Ok((accept, path)) => {
                        println!(
                            "[file] 接收 offer：{}（{} 字节，目录={}）→ {}",
                            display_name(&offer.name),
                            offer.size,
                            offer.is_dir,
                            path.display()
                        );
                        self.stats.accepts_sent += 1;
                        out.push(Envelope::file_accept(
                            accept.transfer_id,
                            accept.resume_offset,
                            accept.accept,
                        ));
                    }
                    Err(err) => {
                        warn!(error = %err, "拒绝 FileOffer");
                        self.stats.files_failed += 1;
                        out.push(Envelope::file_accept(offer.transfer_id, 0, false));
                    }
                }
            }
            Some(Body::FileChunk(chunk)) => {
                self.stats.chunks_received += 1;
                self.stats.bytes += chunk.data.len() as u64;
                match self.receiver.on_chunk(chunk) {
                    Ok(replies) => {
                        for reply in replies {
                            match &reply.body {
                                Some(Body::FileAck(_)) => self.stats.acks_sent += 1,
                                Some(Body::FileResult(result)) => {
                                    self.stats.results_sent += 1;
                                    if result.ok {
                                        self.stats.files_completed += 1;
                                        println!(
                                            "[file] 接收完成（transfer {}，累计 {} 字节）",
                                            result.transfer_id, self.stats.bytes
                                        );
                                    } else {
                                        self.stats.files_failed += 1;
                                        println!(
                                            "[file] 接收失败（transfer {}）：{}",
                                            result.transfer_id, result.error
                                        );
                                    }
                                }
                                _ => {}
                            }
                            out.push(reply);
                        }
                    }
                    Err(err) => {
                        warn!(error = %err, "处理 FileChunk 失败");
                        self.stats.files_failed += 1;
                        out.push(Envelope::file_result(
                            chunk.transfer_id,
                            false,
                            err.to_string(),
                            Vec::new(),
                        ));
                    }
                }
            }
            Some(Body::FileAck(_)) | Some(Body::FileResult(_)) => {
                // Agent 只做接收方，发送方向的消息忽略。
                debug!(kind = env.kind_name(), "Agent 收到发送方向的文件消息，忽略");
            }
            _ => {}
        }
        out
    }

    /// 统计快照。
    pub fn stats(&self) -> FileStats {
        self.stats
    }
}

/// View 侧发送阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// 空闲（可以开始下一个根路径）。
    Idle,
    /// 已发 offer，等 accept。
    AwaitAccept,
    /// 正在发数据块。
    Sending,
    /// 目录展开中：上一个子项完成后继续发下一个子项。
    DirExpand,
}

/// View 侧：按队列把本地文件/目录发给 Agent。
pub struct ViewFiles {
    sender: FileSender,
    queue: VecDeque<PathBuf>,
    current_name: Option<String>,
    current_is_dir: bool,
    stats: FileStats,
    phase: Phase,
}

impl ViewFiles {
    /// `paths` 为要发送的文件/目录（目录会被展开为多个条目）。
    pub fn new(paths: Vec<PathBuf>) -> Self {
        Self {
            sender: FileSender::new(0),
            queue: paths.into(),
            current_name: None,
            current_is_dir: false,
            stats: FileStats::default(),
            phase: Phase::Idle,
        }
    }

    /// 是否所有路径都已处理完。
    pub fn is_done(&self) -> bool {
        self.phase == Phase::Idle && self.queue.is_empty()
    }

    /// 启动下一个根路径的传输（空闲时才生效）；返回需要发出的 offer 帧。
    pub fn start_next(&mut self) -> Vec<Envelope> {
        if self.phase != Phase::Idle {
            return Vec::new();
        }
        let Some(path) = self.queue.pop_front() else {
            return Vec::new();
        };
        match self.offer_root(&path) {
            Ok(env) => {
                self.phase = Phase::AwaitAccept;
                vec![env]
            }
            Err(err) => {
                warn!(path = %path.display(), error = %err, "打开待发送路径失败，跳过");
                self.stats.files_failed += 1;
                self.start_next()
            }
        }
    }

    fn offer_root(&mut self, path: &Path) -> Result<Envelope> {
        let offer = self.sender.offer(path)?;
        self.current_name = Some(offer.name.clone());
        self.current_is_dir = offer.is_dir;
        println!(
            "[file] 发送 offer：{}（{} 字节，目录={}）",
            display_name(&offer.name),
            offer.size,
            offer.is_dir
        );
        self.stats.offers_sent += 1;
        Ok(Envelope::file_offer(
            offer.transfer_id,
            offer.name,
            offer.size,
            offer.last_modified_ms,
            offer.is_dir,
        ))
    }

    fn next_dir_entry(&mut self) -> Vec<Envelope> {
        match self.sender.next_offer() {
            Ok(Some(offer)) => {
                println!(
                    "[file] 目录项 offer：{}（{} 字节，目录={}）",
                    display_name(&offer.name),
                    offer.size,
                    offer.is_dir
                );
                self.stats.offers_sent += 1;
                vec![Envelope::file_offer(
                    offer.transfer_id,
                    offer.name,
                    offer.size,
                    offer.last_modified_ms,
                    offer.is_dir,
                )]
            }
            Ok(None) => {
                println!("[file] 目录展开完毕");
                self.finish_current();
                Vec::new()
            }
            Err(err) => {
                warn!(error = %err, "展开目录失败");
                self.stats.files_failed += 1;
                self.finish_current();
                Vec::new()
            }
        }
    }

    /// 当前项收尾：回到 Idle 并立刻开始下一个根路径。
    fn finish_current(&mut self) {
        self.current_name = None;
        self.current_is_dir = false;
        self.phase = Phase::Idle;
    }

    /// 处理一条来自 Agent 的文件控制帧，返回需要回发的帧。
    pub fn on_envelope(&mut self, env: &Envelope) -> Vec<Envelope> {
        let mut out = Vec::new();
        match &env.body {
            Some(Body::FileAccept(accept)) => {
                self.stats.accepts_received += 1;
                if !accept.accept {
                    println!("[file] 对端拒绝接收（transfer {}）", accept.transfer_id);
                    self.stats.files_failed += 1;
                    self.finish_current();
                    out.extend(self.start_next());
                    return out;
                }
                let accepted_is_dir = self.sender_describes_dir();
                match self.sender.on_accept(accept) {
                    Ok(mut replies) => out.append(&mut replies),
                    Err(err) => {
                        warn!(error = %err, "处理 FileAccept 失败");
                        self.stats.files_failed += 1;
                        self.finish_current();
                        return out;
                    }
                }
                if accepted_is_dir {
                    self.phase = Phase::DirExpand;
                    out.extend(self.next_dir_entry());
                } else {
                    self.phase = Phase::Sending;
                }
            }
            Some(Body::FileAck(ack)) => {
                self.stats.acks_received += 1;
                self.sender.on_ack(ack);
            }
            Some(Body::FileResult(result)) => {
                self.stats.results_received += 1;
                let name = self.current_name.clone().unwrap_or_default();
                let _ = self.sender.on_result(result);
                if result.ok {
                    self.stats.files_completed += 1;
                    println!("[file] 传输完成：{}", display_name(&name));
                } else {
                    self.stats.files_failed += 1;
                    println!("[file] 传输失败：{}：{}", display_name(&name), result.error);
                }
                if self.phase == Phase::DirExpand {
                    out.extend(self.next_dir_entry());
                } else {
                    self.finish_current();
                    out.extend(self.start_next());
                }
            }
            Some(Body::FileOffer(_)) => {
                debug!("View 侧收到 offer（本端只发送），忽略");
            }
            _ => {}
        }
        out
    }

    /// 当前 sender 是否处于「目录 offer 已接受」状态（用于决定是否继续展开）。
    fn sender_describes_dir(&self) -> bool {
        self.current_is_dir
    }

    /// 发送数据块（在途字节超限或等待 ack 时暂停）。
    pub fn poll_chunks(&mut self) -> Vec<Envelope> {
        let mut out = Vec::new();
        if self.phase != Phase::Sending {
            return out;
        }
        for _ in 0..CHUNKS_PER_TICK {
            if self.sender.is_blocked_on_ack()
                || self.sender.in_flight_bytes() >= MAX_IN_FLIGHT_BYTES
            {
                break;
            }
            match self.sender.next_chunk(DEFAULT_CHUNK_BYTES) {
                Ok(Some(env)) => {
                    self.stats.chunks_sent += 1;
                    if let Some(Body::FileChunk(chunk)) = &env.body {
                        self.stats.bytes += chunk.data.len() as u64;
                    }
                    out.push(env);
                }
                Ok(None) => break,
                Err(err) => {
                    warn!(error = %err, "读取待发送块失败");
                    self.stats.files_failed += 1;
                    break;
                }
            }
        }
        out
    }

    /// 统计快照。
    pub fn stats(&self) -> FileStats {
        self.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_envelope_detection() {
        assert!(is_file_envelope(&Envelope::file_offer(
            1, "a.txt", 1, 0, false
        )));
        assert!(is_file_envelope(&Envelope::file_accept(1, 0, true)));
        assert!(is_file_envelope(&Envelope::file_chunk(1, 0, vec![1], true)));
        assert!(is_file_envelope(&Envelope::file_ack(1, 1)));
        assert!(is_file_envelope(&Envelope::file_result(
            1,
            true,
            "",
            vec![0; 32]
        )));
        assert!(!is_file_envelope(&Envelope::clipboard_applied(vec![0; 32])));
    }

    #[test]
    fn display_name_falls_back() {
        assert_eq!(display_name("a.txt"), "a.txt");
        assert_eq!(display_name("  "), "<unnamed>");
    }
}
