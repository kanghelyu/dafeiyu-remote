// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// 大肥鱼远程 —— CF_HDROP（文件列表剪贴板）的编码/解码。
//
// DROPFILES 结构（见 MSDN「DROPFILES」）：
// ```c
// typedef struct _DROPFILES {
//   DWORD pFiles;  // 文件列表相对本结构起点的字节偏移
//   POINT pt;      // 拖放时的鼠标坐标（剪贴板场景无意义，填 0）
//   BOOL  fNC;     // 是否非客户区拖放
//   BOOL  fWide;   // 文件名是否为 UTF-16（TRUE）
// } DROPFILES;
// ```
// 其后是 `fWide` 决定编码的、以 NUL 分隔的文件名列表，最后一个额外 NUL 结尾。
// 剪贴板场景一律使用 UTF-16（`fWide = TRUE`），结构头固定 20 字节。

use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Result};

/// `DROPFILES` 头长度（pFiles 4 + pt 8 + fNC 4 + fWide 4 = 20）。
pub const DROPFILES_HEADER_LEN: usize = 20;

/// 是否宽字符（UTF-16）模式写出的标志值。
const DROPFILES_FWIDE: u32 = 1;

/// 把路径列表编码成 `CF_HDROP` 数据（UTF-16、NUL 分隔、双 NUL 结尾）。
pub fn encode_file_list(paths: &[PathBuf]) -> Vec<u8> {
    let mut names: Vec<u16> = Vec::new();
    for path in paths {
        // Windows 扩展长度路径前缀由调用方决定；这里按原样写出
        let text = path.to_string_lossy();
        names.extend(text.encode_utf16());
        names.push(0);
    }
    if names.is_empty() || names.last() != Some(&0) {
        names.push(0);
    }
    // 列表整体再补一个 NUL，构成「双 NUL 结尾」
    names.push(0);

    let mut out = Vec::with_capacity(DROPFILES_HEADER_LEN + names.len() * 2);
    out.extend_from_slice(&(DROPFILES_HEADER_LEN as u32).to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes()); // pt.x
    out.extend_from_slice(&0i32.to_le_bytes()); // pt.y
    out.extend_from_slice(&0u32.to_le_bytes()); // fNC = FALSE
    out.extend_from_slice(&DROPFILES_FWIDE.to_le_bytes()); // fWide = TRUE
    for unit in names {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out
}

/// 解析 `CF_HDROP` 数据，返回文件路径列表。
///
/// 兼容 `fWide = FALSE`（ANSI/系统代码页）的历史数据：按 Latin-1 近似解码并给出警告级
/// 的错误容忍策略——不静默丢数据，调用方仍能拿到路径（非 ASCII 可能有损）。
pub fn parse_file_list(bytes: &[u8]) -> Result<Vec<PathBuf>> {
    ensure!(
        bytes.len() >= DROPFILES_HEADER_LEN,
        "CF_HDROP 数据过短：{} 字节（至少 {}）",
        bytes.len(),
        DROPFILES_HEADER_LEN
    );
    let p_files = u32::from_le_bytes(bytes[0..4].try_into().expect("4 字节")) as usize;
    let f_wide = u32::from_le_bytes(bytes[16..20].try_into().expect("4 字节")) != 0;
    ensure!(
        p_files >= DROPFILES_HEADER_LEN && p_files <= bytes.len(),
        "CF_HDROP 的 pFiles={p_files} 越界（总长 {}）",
        bytes.len()
    );

    let payload = &bytes[p_files..];
    let paths = if f_wide {
        parse_wide(payload)?
    } else {
        parse_ansi(payload)
    };
    Ok(paths)
}

fn parse_wide(payload: &[u8]) -> Result<Vec<PathBuf>> {
    if payload.len() % 2 != 0 {
        bail!("宽字符 CF_HDROP 负载长度 {} 不是偶数", payload.len());
    }
    let units: Vec<u16> = payload
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    // 末尾可能只有一个 NUL（非法但常见），逐段截取到最后一个非空段
    let mut names = Vec::new();
    let mut start = 0usize;
    for (i, &unit) in units.iter().enumerate() {
        if unit == 0 {
            if i > start {
                let name = String::from_utf16_lossy(&units[start..i]);
                names.push(PathBuf::from(name));
            } else if i == start {
                // 连续两个 NUL：列表结束
                if !names.is_empty() {
                    break;
                }
            }
            start = i + 1;
        }
    }
    Ok(names)
}

fn parse_ansi(payload: &[u8]) -> Vec<PathBuf> {
    let mut names = Vec::new();
    for segment in payload.split(|b| *b == 0) {
        if segment.is_empty() {
            if !names.is_empty() {
                break;
            }
            continue;
        }
        let name: String = segment.iter().map(|b| *b as char).collect();
        names.push(PathBuf::from(name));
    }
    names
}

/// 便捷：单路径编码。
#[allow(dead_code)]
pub fn encode_single_path(path: &Path) -> Vec<u8> {
    encode_file_list(&[path.to_path_buf()])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_ascii_paths() {
        let paths = vec![
            PathBuf::from(r"C:\Users\Public\a.txt"),
            PathBuf::from(r"D:\data\b.bin"),
        ];
        let bytes = encode_file_list(&paths);
        assert_eq!(bytes.len() % 2, 0);
        let parsed = parse_file_list(&bytes).expect("解析失败");
        assert_eq!(parsed, paths);
    }

    #[test]
    fn roundtrip_unicode_paths() {
        let paths = vec![PathBuf::from(r"C:\用户\大肥鱼 远程.txt")];
        let parsed = parse_file_list(&encode_file_list(&paths)).expect("解析失败");
        assert_eq!(parsed, paths);
    }

    #[test]
    fn rejects_short_data() {
        assert!(parse_file_list(b"short").is_err());
    }

    #[test]
    fn empty_list_roundtrips_to_empty() {
        let bytes = encode_file_list(&[]);
        let parsed = parse_file_list(&bytes).expect("解析失败");
        assert!(parsed.is_empty());
    }
}
