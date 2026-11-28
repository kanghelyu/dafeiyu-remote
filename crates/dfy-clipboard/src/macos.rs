// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! macOS 剪贴板后端（基于 `NSPasteboard.generalPasteboard`）。
//!
//! 序列号直接使用 `NSPasteboard.changeCount`；支持文本（UTF-8）、HTML、
//! PNG（及 TIFF 自动转 PNG）、文件列表（`NSURL` ↔ `CF_HDROP`）与 RTF。

use std::ffi::{c_char, CString};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use anyhow::{bail, ensure, Context, Result};

use crate::{
    dib_to_png, encode_file_list, extract_html_fragment, ClipFormat, ClipKind, ClipboardBackend,
    DataBlock, FormatList, HTML_FORMAT_NAME,
};

const KIND_TEXT: u32 = 1;
const KIND_HTML: u32 = 2;
const KIND_PNG: u32 = 3;
const KIND_FILES: u32 = 4;
const KIND_RTF: u32 = 5;

const MASK_TEXT: u32 = 1 << 0;
const MASK_HTML: u32 = 1 << 1;
const MASK_PNG: u32 = 1 << 2;
const MASK_FILES: u32 = 1 << 3;
const MASK_RTF: u32 = 1 << 4;

const ID_TEXT: u32 = 13;
const ID_FILES: u32 = 15;
const ID_HTML: u32 = 0xC001;
const ID_PNG: u32 = 0xC002;
const ID_RTF: u32 = 0xC003;

extern "C" {
    fn dfy_pb_change_count() -> i64;
    fn dfy_pb_available_mask(out_change_count: *mut i64) -> u32;
    fn dfy_pb_read_kind(kind_tag: u32, out_len: *mut usize) -> *mut u8;
    fn dfy_pb_write_bytes(kind_tag: u32, data: *const u8, len: usize) -> i32;
    fn dfy_pb_write_files(paths: *const *const c_char, count: usize) -> i32;
    fn dfy_pb_free(ptr: *mut u8);
}

fn pasteboard_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

pub(crate) fn current_change_count() -> u64 {
    let _guard = pasteboard_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let count = unsafe { dfy_pb_change_count() };
    count.max(0) as u64
}

fn formats_from_mask(mask: u32) -> Vec<ClipFormat> {
    let mut formats = Vec::new();
    if mask & MASK_TEXT != 0 {
        formats.push(ClipFormat {
            id: ID_TEXT,
            name: "CF_UNICODETEXT".to_string(),
            kind: ClipKind::Text,
        });
    }
    if mask & MASK_HTML != 0 {
        formats.push(ClipFormat {
            id: ID_HTML,
            name: HTML_FORMAT_NAME.to_string(),
            kind: ClipKind::Html,
        });
    }
    if mask & MASK_PNG != 0 {
        formats.push(ClipFormat {
            id: ID_PNG,
            name: "PNG".to_string(),
            kind: ClipKind::Png,
        });
    }
    if mask & MASK_FILES != 0 {
        formats.push(ClipFormat {
            id: ID_FILES,
            name: "CF_HDROP".to_string(),
            kind: ClipKind::FileList,
        });
    }
    if mask & MASK_RTF != 0 {
        formats.push(ClipFormat {
            id: ID_RTF,
            name: "Rich Text Format".to_string(),
            kind: ClipKind::Rtf,
        });
    }
    formats
}

fn kind_tag_for_id(format_id: u32) -> Result<u32> {
    match format_id {
        ID_TEXT => Ok(KIND_TEXT),
        ID_HTML => Ok(KIND_HTML),
        ID_PNG => Ok(KIND_PNG),
        ID_FILES => Ok(KIND_FILES),
        ID_RTF => Ok(KIND_RTF),
        other => bail!("macOS 剪贴板不支持未知格式 ID {other}"),
    }
}

fn decode_nul_separated_paths(raw: &[u8]) -> Vec<PathBuf> {
    raw.split(|&b| b == 0)
        .filter(|seg| !seg.is_empty())
        .map(|seg| PathBuf::from(String::from_utf8_lossy(seg).into_owned()))
        .collect()
}

fn slice_block(format_id: u32, payload: &[u8], offset: u64, max: usize) -> DataBlock {
    let total_len = payload.len() as u64;
    let start = offset.min(total_len) as usize;
    let end = if max == 0 {
        start
    } else {
        start.saturating_add(max).min(payload.len())
    };
    DataBlock {
        id: format_id,
        total_len,
        offset,
        block: payload[start..end].to_vec(),
    }
}

/// macOS `NSPasteboard` 剪贴板后端。
#[derive(Debug, Default)]
pub struct MacClipboard;

impl MacClipboard {
    /// 创建 macOS 剪贴板后端实例。
    pub fn new() -> Self {
        Self
    }
}

impl ClipboardBackend for MacClipboard {
    fn sequence(&self) -> Result<u64> {
        Ok(current_change_count())
    }

    fn list_formats(&self) -> Result<FormatList> {
        let _guard = pasteboard_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut count: i64 = 0;
        let mask = unsafe { dfy_pb_available_mask(&mut count) };
        Ok(FormatList {
            sequence: count.max(0) as u64,
            formats: formats_from_mask(mask),
        })
    }

    fn read_block(&self, format_id: u32, offset: u64, max: usize) -> Result<DataBlock> {
        let tag = kind_tag_for_id(format_id)?;
        let _guard = pasteboard_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut len: usize = 0;
        let ptr = unsafe { dfy_pb_read_kind(tag, &mut len) };
        if ptr.is_null() {
            bail!("macOS 剪贴板中没有格式 {format_id}");
        }
        let raw = unsafe {
            let slice = std::slice::from_raw_parts(ptr, len);
            let copied = slice.to_vec();
            dfy_pb_free(ptr);
            copied
        };
        let payload = match format_id {
            ID_FILES => {
                let paths = decode_nul_separated_paths(&raw);
                ensure!(!paths.is_empty(), "macOS 剪贴板文件列表为空");
                encode_file_list(&paths)
            }
            ID_HTML => extract_html_fragment(&raw).to_vec(),
            _ => raw,
        };
        Ok(slice_block(format_id, &payload, offset, max))
    }

    fn write(&mut self, kind: &ClipKind, bytes: &[u8], files: &[PathBuf]) -> Result<()> {
        let _guard = pasteboard_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match kind {
            ClipKind::Text => {
                std::str::from_utf8(bytes).context("macOS 文本剪贴板内容必须是 UTF-8")?;
                let rc = unsafe { dfy_pb_write_bytes(KIND_TEXT, bytes.as_ptr(), bytes.len()) };
                ensure!(rc == 0, "写入 macOS 文本剪贴板失败");
                Ok(())
            }
            ClipKind::Html => {
                std::str::from_utf8(bytes).context("macOS HTML 剪贴板内容必须是 UTF-8")?;
                let rc = unsafe { dfy_pb_write_bytes(KIND_HTML, bytes.as_ptr(), bytes.len()) };
                ensure!(rc == 0, "写入 macOS HTML 剪贴板失败");
                Ok(())
            }
            ClipKind::Png => {
                ensure!(!bytes.is_empty(), "macOS PNG 剪贴板数据不能为空");
                let rc = unsafe { dfy_pb_write_bytes(KIND_PNG, bytes.as_ptr(), bytes.len()) };
                ensure!(rc == 0, "写入 macOS PNG 剪贴板失败");
                Ok(())
            }
            ClipKind::Dib => {
                let png = dib_to_png(bytes).context("DIB 转 PNG 失败")?;
                let rc = unsafe { dfy_pb_write_bytes(KIND_PNG, png.as_ptr(), png.len()) };
                ensure!(rc == 0, "写入 macOS PNG(DIB) 剪贴板失败");
                Ok(())
            }
            ClipKind::Rtf => {
                let rc = unsafe { dfy_pb_write_bytes(KIND_RTF, bytes.as_ptr(), bytes.len()) };
                ensure!(rc == 0, "写入 macOS RTF 剪贴板失败");
                Ok(())
            }
            ClipKind::FileList => {
                ensure!(!files.is_empty(), "macOS 文件剪贴板列表不能为空");
                let cstrings: Vec<CString> = files
                    .iter()
                    .map(|p| {
                        CString::new(p.to_string_lossy().as_bytes())
                            .context("文件路径包含 NUL 字节")
                    })
                    .collect::<Result<Vec<_>>>()?;
                let ptrs: Vec<*const c_char> = cstrings.iter().map(|s| s.as_ptr()).collect();
                let rc = unsafe { dfy_pb_write_files(ptrs.as_ptr(), ptrs.len()) };
                ensure!(rc == 0, "写入 macOS 文件列表剪贴板失败");
                Ok(())
            }
            ClipKind::Other(name) => {
                bail!("macOS 剪贴板暂不支持自定义格式：{name}")
            }
        }
    }
}

/// 创建 macOS `NSPasteboard` 剪贴板后端。
pub fn create_backend() -> Result<Box<dyn ClipboardBackend>> {
    Ok(Box::new(MacClipboard::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_maps_to_canonical_protocol_formats() {
        let formats = formats_from_mask(MASK_TEXT | MASK_HTML | MASK_PNG | MASK_FILES | MASK_RTF);
        assert_eq!(formats.len(), 5);
        assert_eq!(formats[0].id, ID_TEXT);
        assert_eq!(formats[0].name, "CF_UNICODETEXT");
        assert_eq!(formats[0].kind, ClipKind::Text);
        assert_eq!(formats[1].id, ID_HTML);
        assert_eq!(formats[1].name, HTML_FORMAT_NAME);
        assert_eq!(formats[2].id, ID_PNG);
        assert_eq!(formats[2].name, "PNG");
        assert_eq!(formats[3].id, ID_FILES);
        assert_eq!(formats[3].name, "CF_HDROP");
        assert_eq!(formats[4].id, ID_RTF);
        assert_eq!(formats[4].name, "Rich Text Format");
    }

    #[test]
    fn slice_block_handles_probe_chunk_and_eof() {
        let payload = b"hello world";
        let probe = slice_block(ID_TEXT, payload, 0, 0);
        assert_eq!(probe.total_len, 11);
        assert!(probe.block.is_empty());

        let head = slice_block(ID_TEXT, payload, 0, 5);
        assert_eq!(head.block, b"hello");

        let tail = slice_block(ID_TEXT, payload, 6, 100);
        assert_eq!(tail.block, b"world");

        let past = slice_block(ID_TEXT, payload, 99, 10);
        assert!(past.block.is_empty());
        assert_eq!(past.total_len, 11);
    }

    #[test]
    fn nul_separated_paths_decode_and_encode_hdrop() {
        let raw = b"/tmp/a.txt\0/tmp/b dir/c.png\0";
        let paths = decode_nul_separated_paths(raw);
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/tmp/a.txt"),
                PathBuf::from("/tmp/b dir/c.png")
            ]
        );
        let hdrop = encode_file_list(&paths);
        let roundtrip = crate::parse_file_list(&hdrop).unwrap();
        assert_eq!(roundtrip, paths);
    }

    #[test]
    fn pasteboard_sequence_and_list_formats_are_non_destructive() {
        let backend = create_backend().expect("macOS pasteboard backend");
        let seq = backend.sequence().expect("read changeCount");
        let list = backend.list_formats().expect("list formats");
        assert!(list.sequence >= seq);
    }

    #[test]
    #[ignore = "mutates system pasteboard"]
    fn live_pasteboard_text_html_png_roundtrip() {
        let mut backend = create_backend().unwrap();
        let before = backend.sequence().unwrap();

        let text = "大肥鱼 macOS 剪贴板测试";
        backend
            .write(&ClipKind::Text, text.as_bytes(), &[])
            .unwrap();
        let seq1 = backend.sequence().unwrap();
        assert!(seq1 > before);
        let list = backend.list_formats().unwrap();
        assert!(list.contains_kind(&ClipKind::Text));
        let block = backend.read_block(ID_TEXT, 0, 1024).unwrap();
        assert_eq!(block.block, text.as_bytes());

        let html = "<b>hello</b>";
        backend
            .write(&ClipKind::Html, html.as_bytes(), &[])
            .unwrap();
        let list = backend.list_formats().unwrap();
        assert!(list.contains_kind(&ClipKind::Html));
        let block = backend.read_block(ID_HTML, 0, 1024).unwrap();
        assert_eq!(block.block, html.as_bytes());

        // Minimal 1x1 RGBA PNG
        let mut png_bytes = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut png_bytes, 1, 1);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().unwrap();
            writer.write_image_data(&[10, 20, 30, 255]).unwrap();
        }
        backend.write(&ClipKind::Png, &png_bytes, &[]).unwrap();
        let list = backend.list_formats().unwrap();
        assert!(list.contains_kind(&ClipKind::Png));
        let block = backend.read_block(ID_PNG, 0, 65536).unwrap();
        assert!(!block.block.is_empty());
    }
}
