// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 剪贴板**同步状态机**（拉模式，`spec/DFYP-v1.md` §8）。
//!
//! 该模块只做协议侧状态机，不关心传输：调用方把 [`SyncMessage`] 变成 `dfy-proto` 的
//! `Envelope` 发出去，把收到的 `Envelope` 解成 [`SyncMessage`] 喂给 `on_remote`。
//!
//! 设计要点：
//! 1. **清单 → 按需拉块**：来源端只广播 `FormatList`（格式 id / BLAKE3 哈希 / 大小 / 名字），
//!    接收端发现内容不同才 `Pull`，避免每次剪贴板变化都推全量数据。
//! 2. **自激抑制**：本端写入剪贴板后记录 [`LoopGuard`] 的序列号，避免把「自己刚写的内容」
//!    再广播回去形成回环。
//! 3. **单格式、单文件对象**：v1 只同步 4 类载荷（文本 / HTML / PNG / 文件列表），
//!    超过 [`MAX_SYNC_BYTES`] 的对象只同步描述符（大对象留待 file 通道，见 spec §8.3）。
//! 4. **去重**：`Applied{hash}` 让来源端知道对端已写入，避免重复拉取。

use std::collections::HashMap;

use anyhow::{bail, Context, Result};

use crate::{ClipKind, ClipboardBackend, DataBlock, FormatList, LoopGuard};

/// v1 同步的最大对象大小（1 MiB）；超过只发清单项、不发数据。
pub const MAX_SYNC_BYTES: u64 = 1024 * 1024;

/// 拉取分块大小（128 KiB，与 spec §8.2 一致）。
pub const CHUNK_BYTES: usize = 128 * 1024;

/// 协议侧消息（与 `dfy-proto` 的 Clipboard* 一一对应；这里用固定 32B 哈希便于查表）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncMessage {
    /// 格式清单。
    FormatList {
        /// 来源端单调序号。
        seq: u64,
        /// 清单项。
        items: Vec<ClipFormatItem>,
    },
    /// 按哈希拉取某个格式的数据。
    Pull {
        /// 目标格式哈希（BLAKE3，32B）。
        hash: [u8; 32],
    },
    /// 数据分块。
    DataChunk {
        /// 目标格式哈希。
        hash: [u8; 32],
        /// 本块起始偏移。
        offset: u64,
        /// 本块数据。
        data: Vec<u8>,
        /// 是否最后一块。
        last: bool,
    },
    /// 已写入确认。
    Applied {
        /// 已写入的格式哈希。
        hash: [u8; 32],
    },
}

/// 清单项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipFormatItem {
    /// 平台格式 ID。
    pub id: u32,
    /// 内容哈希（BLAKE3，32B）。
    pub hash: [u8; 32],
    /// 内容总长（字节，0=未知/超限）。
    pub size_hint: u64,
    /// 格式名（`CF_UNICODETEXT` / `HTML Format` / `PNG` / `CF_HDROP` …）。
    pub name: String,
}

impl ClipFormatItem {
    /// 是否为 v1 支持的载荷类型。
    #[must_use]
    pub fn is_supported(&self) -> bool {
        kind_from_name(&self.name).is_some() && self.size_hint <= MAX_SYNC_BYTES
    }

    /// 转化为 `dfy-proto` 的清单项。
    #[must_use]
    pub fn to_proto(&self) -> dfy_proto::ClipboardFormat {
        dfy_proto::ClipboardFormat {
            id: self.id,
            hash: self.hash.to_vec(),
            size_hint: u32::try_from(self.size_hint).unwrap_or(0),
            name: self.name.clone(),
        }
    }

    /// 从 `dfy-proto` 的清单项解析；哈希长度不是 32B 时返回 `None`。
    #[must_use]
    pub fn from_proto(p: &dfy_proto::ClipboardFormat) -> Option<Self> {
        Some(Self {
            id: p.id,
            hash: hash_from_slice(&p.hash)?,
            size_hint: u64::from(p.size_hint),
            name: p.name.clone(),
        })
    }
}

/// 内容哈希：BLAKE3-256。
#[must_use]
pub fn hash_bytes(bytes: &[u8]) -> [u8; 32] {
    blake3::hash(bytes).into()
}

fn hash_from_slice(bytes: &[u8]) -> Option<[u8; 32]> {
    <[u8; 32]>::try_from(bytes).ok()
}

/// 由格式名推断 v1 载荷种类；不支持的返回 `None`。
#[must_use]
pub fn kind_from_name(name: &str) -> Option<ClipKind> {
    match name {
        "CF_UNICODETEXT" | "CF_TEXT" | "CF_OEMTEXT" => Some(ClipKind::Text),
        "HTML Format" => Some(ClipKind::Html),
        "PNG" => Some(ClipKind::Png),
        "CF_HDROP" => Some(ClipKind::FileList),
        "Rich Text Format" => Some(ClipKind::Rtf),
        _ => None,
    }
}

/// 优先级：文本 > HTML > PNG > 文件列表（与 spec §8.5 的默认策略一致）。
fn kind_priority(kind: &ClipKind) -> u8 {
    match kind {
        ClipKind::Text => 0,
        ClipKind::Html => 1,
        ClipKind::Png => 2,
        ClipKind::FileList => 3,
        ClipKind::Dib => 4,
        ClipKind::Rtf => 5,
        ClipKind::Other(_) => 6,
    }
}

/// 正在组装的远端对象。
struct Assembling {
    buffer: Vec<u8>,
    expect_offset: u64,
    name: String,
}

/// 剪贴板同步状态机。
pub struct ClipboardSync {
    backend: Box<dyn ClipboardBackend>,
    guard: LoopGuard,
    /// 本端已广播出去的「格式 id → 内容哈希」签名，用于判断是否需要再广播。
    published: HashMap<u32, [u8; 32]>,
    /// 本端已广播过的格式名（收到 Pull 时按哈希反查 id）。
    published_names: HashMap<[u8; 32], u32>,
    /// 远端最近一次清单（哈希 → 名字），收到数据块时用来决定写回种类。
    remote_items: HashMap<[u8; 32], String>,
    /// 远端清单序号，防止乱序的旧清单覆盖新清单。
    remote_seq: u64,
    /// 已从远端接受并写入的哈希，避免重复拉取。
    applied: HashMap<[u8; 32], ()>,
    /// 当前正在组装的远端对象。
    assembling: Option<Assembling>,
    /// 本端上一次观察到的剪贴板序列号（用于节流/忽略自写）。
    last_seq: Option<u64>,
}

impl ClipboardSync {
    /// 新建状态机。
    pub fn new(backend: Box<dyn ClipboardBackend>) -> Self {
        Self {
            backend,
            guard: LoopGuard::new(),
            published: HashMap::new(),
            published_names: HashMap::new(),
            remote_items: HashMap::new(),
            remote_seq: 0,
            applied: HashMap::new(),
            assembling: None,
            last_seq: None,
        }
    }

    /// 后端引用（测试/诊断用）。
    #[must_use]
    pub fn backend(&self) -> &dyn ClipboardBackend {
        self.backend.as_ref()
    }

    /// 轮询本地剪贴板；有变化且值得同步时返回 `FormatList`。
    ///
    /// 无变化、或内容与上次广播相同 → 返回 `Ok(None)`（不产生流量）。
    pub fn poll_local(&mut self) -> Result<Option<SyncMessage>> {
        let list = self.backend.list_formats().context("枚举剪贴板格式")?;
        let seq = list.sequence;
        if self.last_seq == Some(seq) || self.guard.should_ignore(seq) {
            return Ok(None);
        }
        self.last_seq = Some(seq);

        let items = self.collect_supported_items(&list)?;
        if items.is_empty() {
            return Ok(None);
        }
        let signature: HashMap<u32, [u8; 32]> = items.iter().map(|it| (it.id, it.hash)).collect();
        if signature == self.published {
            return Ok(None);
        }
        self.published = signature;
        self.published_names = items.iter().map(|it| (it.hash, it.id)).collect();
        Ok(Some(SyncMessage::FormatList { seq, items }))
    }

    /// 收集可同步的格式并按「每类一个、优先级最高」筛选。
    fn collect_supported_items(&mut self, list: &FormatList) -> Result<Vec<ClipFormatItem>> {
        // 每类只取优先级最高（系统格式在前）的那个，避免同内容多格式重复传输。
        let mut best: HashMap<u8, (usize, ClipFormatItem)> = HashMap::new();
        for f in &list.formats {
            let Some(kind) = kind_from_name(&f.name) else {
                continue;
            };
            let prio = kind_priority(&kind);
            if best.contains_key(&prio) {
                continue;
            }
            let block = match self.backend.read_block(f.id, 0, 0) {
                Ok(b) => b,
                Err(err) => {
                    tracing::debug!(format = %f.name, %err, "读取剪贴板格式大小失败，跳过");
                    continue;
                }
            };
            if block.total_len > MAX_SYNC_BYTES {
                continue;
            }
            let data = match self.backend.read_block(
                f.id,
                0,
                usize::try_from(block.total_len).unwrap_or(0),
            ) {
                Ok(b) => b.block,
                Err(err) => {
                    tracing::debug!(format = %f.name, %err, "读取剪贴板数据失败，跳过");
                    continue;
                }
            };
            let item = ClipFormatItem {
                id: f.id,
                hash: hash_bytes(&data),
                size_hint: data.len() as u64,
                name: f.name.clone(),
            };
            best.insert(prio, (f.id as usize, item));
        }
        let mut items: Vec<ClipFormatItem> = best.into_values().map(|(_, it)| it).collect();
        items.sort_by_key(|it| kind_from_name(&it.name).map(|k| kind_priority(&k)));
        Ok(items)
    }

    /// 处理来自远端的一条消息，返回需要回给远端的消息（可能 0 条或多条）。
    pub fn on_remote(&mut self, msg: &SyncMessage) -> Result<Vec<SyncMessage>> {
        match msg {
            SyncMessage::FormatList { seq, items } => self.on_remote_list(*seq, items),
            SyncMessage::Pull { hash } => self.on_remote_pull(*hash),
            SyncMessage::DataChunk {
                hash,
                offset,
                data,
                last,
            } => self.on_remote_chunk(*hash, *offset, data, *last),
            SyncMessage::Applied { hash } => {
                self.applied.insert(*hash, ());
                Ok(Vec::new())
            }
        }
    }

    fn on_remote_list(&mut self, seq: u64, items: &[ClipFormatItem]) -> Result<Vec<SyncMessage>> {
        if seq < self.remote_seq {
            return Ok(Vec::new());
        }
        self.remote_seq = seq;
        for it in items.iter().filter(|it| it.is_supported()) {
            self.remote_items.insert(it.hash, it.name.clone());
        }
        // 选择优先级最高的可支持项；与本端已应用/已发布的内容相同则跳过。
        let mut best: Option<&ClipFormatItem> = None;
        for it in items.iter().filter(|it| it.is_supported()) {
            if self.applied.contains_key(&it.hash) {
                continue;
            }
            let replace = match best {
                None => true,
                Some(cur) => {
                    let k_new = kind_from_name(&it.name)
                        .map(|k| kind_priority(&k))
                        .unwrap_or(9);
                    let k_cur = kind_from_name(&cur.name)
                        .map(|k| kind_priority(&k))
                        .unwrap_or(9);
                    k_new < k_cur
                }
            };
            if replace {
                best = Some(it);
            }
        }
        let Some(it) = best else {
            return Ok(Vec::new());
        };
        // 若本端已是同样内容（哈希相同），不必拉取。
        if self.published_names.contains_key(&it.hash) {
            return Ok(Vec::new());
        }
        Ok(vec![SyncMessage::Pull { hash: it.hash }])
    }

    fn on_remote_pull(&mut self, hash: [u8; 32]) -> Result<Vec<SyncMessage>> {
        let Some(&format_id) = self.published_names.get(&hash) else {
            // 本端没有这个内容（可能已被覆盖/过期）：静默忽略，让对端保留旧内容。
            tracing::debug!("收到未知 hash 的剪贴板拉取请求，忽略");
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        let mut offset: u64 = 0;
        loop {
            let block: DataBlock = self.backend.read_block(format_id, offset, CHUNK_BYTES)?;
            if block.block.is_empty() && offset < block.total_len {
                bail!(
                    "剪贴板数据读取提前结束（offset={offset}, total={}）",
                    block.total_len
                );
            }
            let is_last = offset + block.block.len() as u64 >= block.total_len;
            out.push(SyncMessage::DataChunk {
                hash,
                offset,
                data: block.block,
                last: is_last,
            });
            offset += out
                .last()
                .map(|m| match m {
                    SyncMessage::DataChunk { data, .. } => data.len() as u64,
                    _ => 0,
                })
                .unwrap_or(0);
            if is_last {
                break;
            }
            if out.len() > 4096 {
                bail!("剪贴板拉取分块过多，疑似对象持续增长");
            }
        }
        Ok(out)
    }

    fn on_remote_chunk(
        &mut self,
        hash: [u8; 32],
        offset: u64,
        data: &[u8],
        last: bool,
    ) -> Result<Vec<SyncMessage>> {
        let name = self
            .remote_items
            .get(&hash)
            .cloned()
            .unwrap_or_else(|| "CF_UNICODETEXT".to_string());
        match &mut self.assembling {
            Some(asm) if asm.expect_offset == offset && asm.name == name => {
                asm.expect_offset = offset + data.len() as u64;
                asm.buffer.extend_from_slice(data);
            }
            _ => {
                let mut buffer = Vec::with_capacity(data.len());
                buffer.extend_from_slice(data);
                self.assembling = Some(Assembling {
                    buffer,
                    expect_offset: offset + data.len() as u64,
                    name,
                });
            }
        }
        if !last {
            return Ok(Vec::new());
        }
        let Some(asm) = self.assembling.take() else {
            return Ok(Vec::new());
        };
        if asm.buffer.len() as u64 > MAX_SYNC_BYTES {
            bail!("远端剪贴板对象超过 v1 上限");
        }
        self.write_remote(&asm.name, &asm.buffer)?;
        self.guard.note_write();
        self.applied.insert(hash, ());
        Ok(vec![SyncMessage::Applied { hash }])
    }

    fn write_remote(&mut self, name: &str, bytes: &[u8]) -> Result<()> {
        match kind_from_name(name) {
            Some(ClipKind::FileList) => {
                let files = crate::parse_file_list(bytes).context("解析远端文件列表")?;
                self.backend.write(&ClipKind::FileList, &[], &files)
            }
            Some(kind) => self.backend.write(&kind, bytes, &[]),
            None => {
                tracing::debug!(format = name, "未知格式，忽略远端剪贴板对象");
                Ok(())
            }
        }
    }
}

/// 把 `dfy-proto::ClipboardFormatList` 转成 [`SyncMessage`]。
#[must_use]
pub fn message_from_proto_list(p: &dfy_proto::ClipboardFormatList) -> SyncMessage {
    SyncMessage::FormatList {
        seq: p.seq,
        items: p
            .items
            .iter()
            .filter_map(ClipFormatItem::from_proto)
            .collect(),
    }
}

/// 把 [`SyncMessage`] 转成 `dfy-proto::Envelope`；无法表示的（哈希长度错误）返回 `None`。
#[must_use]
pub fn message_to_envelope(msg: &SyncMessage) -> Option<dfy_proto::Envelope> {
    Some(match msg {
        SyncMessage::FormatList { seq, items } => dfy_proto::Envelope::clipboard_format_list(
            *seq,
            items.iter().map(ClipFormatItem::to_proto).collect(),
        ),
        SyncMessage::Pull { hash } => dfy_proto::Envelope::clipboard_pull(hash.to_vec()),
        SyncMessage::DataChunk {
            hash,
            offset,
            data,
            last,
        } => dfy_proto::Envelope::clipboard_data_chunk(
            hash.to_vec(),
            u32::try_from(*offset).ok()?,
            data.clone(),
            *last,
        ),
        SyncMessage::Applied { hash } => dfy_proto::Envelope::clipboard_applied(hash.to_vec()),
    })
}

/// 从 `dfy-proto::Envelope` 解析剪贴板消息；非剪贴板消息返回 `None`。
#[must_use]
pub fn message_from_envelope(env: &dfy_proto::Envelope) -> Option<SyncMessage> {
    use dfy_proto::envelope::Body;
    match env.body.as_ref()? {
        Body::ClipboardFormatList(p) => Some(message_from_proto_list(p)),
        Body::ClipboardPull(p) => Some(SyncMessage::Pull {
            hash: hash_from_slice(&p.hash)?,
        }),
        Body::ClipboardDataChunk(p) => Some(SyncMessage::DataChunk {
            hash: hash_from_slice(&p.hash)?,
            offset: u64::from(p.offset),
            data: p.data.clone(),
            last: p.last,
        }),
        Body::ClipboardApplied(p) => Some(SyncMessage::Applied {
            hash: hash_from_slice(&p.hash)?,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::ClipFormat;

    /// 内存剪贴板后端（测试用）。
    struct MemBackend {
        seq: u64,
        formats: Vec<(u32, String, Vec<u8>)>,
        written: Vec<(String, Vec<u8>)>,
    }

    impl MemBackend {
        fn empty() -> Self {
            Self {
                seq: 0,
                formats: Vec::new(),
                written: Vec::new(),
            }
        }

        fn set_text(&mut self, text: &str) {
            self.seq += 1;
            let bytes = text.as_bytes().to_vec();
            let len = bytes.len() as u32;
            self.formats.clear();
            self.formats.push((13, "CF_UNICODETEXT".into(), bytes));
            // 加一个重复格式验证「每类只取一个」
            self.formats
                .push((13, "CF_TEXT".into(), format!("{len}").into_bytes()));
        }

        fn raw_of(&self, id: u32) -> Option<&Vec<u8>> {
            self.formats
                .iter()
                .find(|(fid, _, _)| *fid == id)
                .map(|(_, _, d)| d)
        }
    }

    impl ClipboardBackend for MemBackend {
        fn sequence(&self) -> Result<u64> {
            Ok(self.seq)
        }

        fn list_formats(&self) -> Result<FormatList> {
            Ok(FormatList {
                sequence: self.seq,
                formats: self
                    .formats
                    .iter()
                    .map(|(id, name, _)| ClipFormat {
                        id: *id,
                        name: name.clone(),
                        kind: kind_from_name(name).unwrap_or(ClipKind::Other(name.clone())),
                    })
                    .collect(),
            })
        }

        fn read_block(&self, format_id: u32, offset: u64, max: usize) -> Result<DataBlock> {
            let data = self.raw_of(format_id).context("格式不存在")?.clone();
            let total = data.len() as u64;
            let start = usize::try_from(offset)
                .unwrap_or(usize::MAX)
                .min(data.len());
            let end = if max == 0 {
                start
            } else {
                (start + max).min(data.len())
            };
            Ok(DataBlock {
                id: format_id,
                total_len: total,
                offset,
                block: data[start..end].to_vec(),
            })
        }

        fn write(&mut self, kind: &ClipKind, bytes: &[u8], files: &[PathBuf]) -> Result<()> {
            let name = match kind {
                ClipKind::Text => "CF_UNICODETEXT",
                ClipKind::Html => "HTML Format",
                ClipKind::Png => "PNG",
                ClipKind::FileList => "CF_HDROP",
                ClipKind::Dib => "CF_DIB",
                ClipKind::Rtf => "Rich Text Format",
                ClipKind::Other(n) => n.as_str(),
            }
            .to_string();
            self.seq += 1;
            if !files.is_empty() {
                self.written.push((name, crate::encode_file_list(files)));
            } else {
                self.written.push((name, bytes.to_vec()));
            }
            Ok(())
        }
    }

    #[test]
    fn text_roundtrip_via_pull() {
        // A 产生文本（A 的 MemBackend 需要能设置内容）
        let mut backend = MemBackend::empty();
        backend.set_text("你好，大肥鱼");
        let mut a = ClipboardSync::new(Box::new(backend));
        let mut b = ClipboardSync::new(Box::new(MemBackend::empty()));

        let list = a.poll_local().unwrap().expect("应有 FormatList");
        let msgs = b.on_remote(&list).unwrap();
        assert_eq!(msgs.len(), 1, "应发出一个 Pull");
        let chunks = a.on_remote(&msgs[0]).unwrap();
        assert!(!chunks.is_empty(), "应返回数据块");
        let mut applier = Vec::new();
        for c in &chunks {
            applier.extend(b.on_remote(c).unwrap());
        }
        assert_eq!(applier.len(), 1, "最后一块应回 Applied");
        assert!(matches!(applier[0], SyncMessage::Applied { .. }));
    }

    #[test]
    fn same_content_does_not_pull() {
        let mut backend = MemBackend::empty();
        backend.set_text("same");
        let mut a = ClipboardSync::new(Box::new(backend));
        let mut backend_b = MemBackend::empty();
        backend_b.set_text("same");
        let mut b = ClipboardSync::new(Box::new(backend_b));

        let list_a = a.poll_local().unwrap().unwrap();
        // B 先本地广播一次，随后收到 A 的清单
        let _ = b.poll_local().unwrap();
        let msgs = b.on_remote(&list_a).unwrap();
        assert!(msgs.is_empty(), "内容相同不应拉取");
    }

    #[test]
    fn oversized_is_skipped_in_listing() {
        let mut backend = MemBackend::empty();
        backend.set_text(&"x".repeat((MAX_SYNC_BYTES + 1) as usize));
        let mut a = ClipboardSync::new(Box::new(backend));
        assert!(a.poll_local().unwrap().is_none(), "超限对象不应广播");
    }

    #[test]
    fn kind_from_name_mapping() {
        assert_eq!(kind_from_name("CF_UNICODETEXT"), Some(ClipKind::Text));
        assert_eq!(kind_from_name("HTML Format"), Some(ClipKind::Html));
        assert_eq!(kind_from_name("PNG"), Some(ClipKind::Png));
        assert_eq!(kind_from_name("CF_HDROP"), Some(ClipKind::FileList));
        assert_eq!(kind_from_name("Something Else"), None);
    }

    #[test]
    fn proto_envelope_roundtrip() {
        let msg = SyncMessage::FormatList {
            seq: 7,
            items: vec![ClipFormatItem {
                id: 13,
                hash: hash_bytes(b"hello"),
                size_hint: 5,
                name: "CF_UNICODETEXT".into(),
            }],
        };
        let env = message_to_envelope(&msg).unwrap();
        assert_eq!(env.kind_name(), "ClipboardFormatList");
        let back = message_from_envelope(&env).unwrap();
        assert_eq!(back, msg);

        let chunk = SyncMessage::DataChunk {
            hash: hash_bytes(b"abc"),
            offset: 0,
            data: b"abc".to_vec(),
            last: true,
        };
        let env = message_to_envelope(&chunk).unwrap();
        assert_eq!(message_from_envelope(&env).unwrap(), chunk);
    }
}
