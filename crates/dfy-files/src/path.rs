// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 协议路径净化与接收根约束。
//!
//! 协议里的 `FileOffer.name` 是对端给的**相对路径**，必须当成不可信输入：
//! 这里手写字符串校验（不依赖 `windows` crate，保持跨平台），拒绝
//! `..`、绝对路径/盘符、UNC、Windows 设备名、控制字符、NTFS 备用数据流，
//! 以及末尾点/空格这类 Windows 会悄悄吞掉的怪名。接收端再把净化后的相对
//! 路径拼到沙箱根下，并逐级拒绝符号链接（防目录逃逸）。

use std::fs;
use std::path::{Path, PathBuf};

use crate::FileError;

/// Windows 保留设备名（不含扩展名时也不允许；`aux.txt` 同样被系统特殊对待）。
const WINDOWS_RESERVED_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Windows 文件名禁用字符（路径分隔符单独处理）。
const FORBIDDEN_CHARS: [char; 7] = ['<', '>', ':', '"', '|', '?', '*'];

/// 单个路径组件的字节上限（NTFS/ext4 常见 255 字节限制）。
const MAX_COMPONENT_BYTES: usize = 255;

/// 净化协议中的相对路径（接收端 `on_offer`、发送端目录展开共用）。
///
/// 返回以 `/` 分隔的规范化相对路径（`\` 统一换成 `/`）。
///
/// 拒绝规则（任一命中即 [`FileError::InvalidPath`]）：
///
/// 1. 空路径、空组件（含尾部 `/`、`a//b`）；
/// 2. 绝对路径（`/etc/passwd`）、UNC/设备路径（`\\server\share`、`\\?\C:\…`）；
/// 3. 盘符或 NTFS 备用数据流（组件含 `:`，如 `C:\Windows\evil`、`file.txt:stream`）；
/// 4. `.` 与 `..`（堵死目录穿越）；
/// 5. 控制字符（`is_control`）与 Windows 禁用字符 `< > : " | ? *`；
/// 6. 以 `.` 或空格结尾的组件（Windows 会吞掉后缀，造成同名覆盖）；
/// 7. Windows 保留设备名（大小写不敏感，含带扩展名形式）；
/// 8. 组件超过 255 字节（UTF-8）。
///
/// 中文名等其它 Unicode 字符原样保留。
pub fn sanitize_relative_path(raw: &str) -> Result<String, FileError> {
    if raw.is_empty() {
        return Err(FileError::invalid("路径为空"));
    }
    // 先统一分隔符，让 `a\b` 与 `a/b` 行为一致；Windows 风格的绝对路径会在下一步被拒。
    let normalized = raw.replace('\\', "/");
    if normalized.starts_with('/') {
        return Err(FileError::invalid(format!("拒绝绝对路径: {raw}")));
    }
    let mut parts: Vec<&str> = Vec::new();
    for part in normalized.split('/') {
        if part.is_empty() {
            return Err(FileError::invalid(format!("路径含空组件: {raw}")));
        }
        validate_component(part)?;
        parts.push(part);
    }
    Ok(parts.join("/"))
}

/// 净化单个文件名（发送端 `offer` 只上送文件名，绝不上送本机绝对路径）。
///
/// 规则与 [`sanitize_relative_path`] 的单个组件一致，但要求输入不含分隔符。
pub fn sanitize_file_name(raw: &str) -> Result<String, FileError> {
    if raw.contains('/') || raw.contains('\\') {
        return Err(FileError::invalid(format!("文件名不得含路径分隔符: {raw}")));
    }
    validate_component(raw)?;
    Ok(raw.to_string())
}

/// 校验单个路径组件。
fn validate_component(part: &str) -> Result<(), FileError> {
    if part.is_empty() {
        return Err(FileError::invalid("路径组件为空"));
    }
    if part == "." || part == ".." {
        return Err(FileError::invalid(format!("拒绝相对路径穿越: {part}")));
    }
    if part.ends_with('.') || part.ends_with(' ') {
        return Err(FileError::invalid(format!(
            "拒绝以点/空格结尾的组件（Windows 会吞掉后缀）: {part}"
        )));
    }
    if part.len() > MAX_COMPONENT_BYTES {
        return Err(FileError::invalid(format!(
            "路径组件过长: {} 字节 > {MAX_COMPONENT_BYTES}",
            part.len()
        )));
    }
    for ch in part.chars() {
        if ch.is_control() {
            return Err(FileError::invalid(format!("拒绝控制字符: {part:?}")));
        }
        if FORBIDDEN_CHARS.contains(&ch) {
            return Err(FileError::invalid(format!("拒绝禁用字符 {ch:?}: {part}")));
        }
    }
    if is_windows_reserved(part) {
        return Err(FileError::invalid(format!(
            "拒绝 Windows 保留设备名: {part}"
        )));
    }
    Ok(())
}

/// 是否为 Windows 保留设备名（取第一个 `.` 之前的部分，大小写不敏感）。
fn is_windows_reserved(part: &str) -> bool {
    let base = part.split('.').next().unwrap_or(part);
    let upper = base.to_ascii_uppercase();
    WINDOWS_RESERVED_NAMES.contains(&upper.as_str())
}

/// 把**已净化**的相对路径解析到接收根目录下，并逐级拒绝符号链接（防逃逸）。
///
/// 即使调用方忘了先净化，这里也会重新校验每个组件（`.`,`..`,`:`,绝对前缀都会被拒）。
/// 注意：这是词法 + 逐级 `symlink_metadata` 检查，不要求目标已存在；
/// Windows junction 只有部分能通过 `is_symlink` 识别（见设计文档「已知限制」），
/// 因此调用方仍应把接收根设为专用目录（例如 `%USERPROFILE%\Downloads\大肥鱼接收`）。
pub(crate) fn resolve_within(root: &Path, relative: &str) -> Result<PathBuf, FileError> {
    if relative.is_empty() {
        return Err(FileError::invalid("相对路径为空"));
    }
    let mut path = root.to_path_buf();
    for part in relative.split('/') {
        // 逐组件复检：调用方即使绕过 sanitize_relative_path 也无法越界。
        validate_component(part)?;
        path.push(part);
        match fs::symlink_metadata(&path) {
            Ok(md) if md.file_type().is_symlink() => {
                return Err(FileError::invalid(format!(
                    "路径包含符号链接, 拒绝写入: {}",
                    path.display()
                )));
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(FileError::io(&path, err)),
        }
    }
    // 词法双保险：净化后不可能含 `..`，仍校验前缀包含关系。
    if !path.starts_with(root) {
        return Err(FileError::invalid(format!(
            "目标逃逸接收根: {}",
            path.display()
        )));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_plain_and_chinese_names() {
        assert_eq!(sanitize_relative_path("a.txt").unwrap(), "a.txt");
        assert_eq!(
            sanitize_relative_path("子目录/文件 名.txt").unwrap(),
            "子目录/文件 名.txt"
        );
        // 反斜杠统一成正斜杠。
        assert_eq!(sanitize_relative_path(r"a\b\c.txt").unwrap(), "a/b/c.txt");
    }

    #[test]
    fn rejects_dotdot_and_absolute() {
        for raw in [
            "../../evil",
            "..",
            "a/../../b",
            "/etc/passwd",
            r"C:\Windows\evil",
            r"C:relative",
            r"\\server\share\evil",
            r"\\?\C:\evil",
            "a//b",
            "a/",
            "",
        ] {
            let err = sanitize_relative_path(raw).unwrap_err();
            assert!(
                matches!(err, FileError::InvalidPath(_)),
                "{raw:?} 应被拒绝, 实际 {err:?}"
            );
        }
    }

    #[test]
    fn rejects_windows_reserved_names() {
        for raw in [
            "aux.txt", "CON", "con.txt", "NUL", "com1", "LPT9.log", "prn.",
        ] {
            let err = sanitize_relative_path(raw).unwrap_err();
            assert!(
                matches!(err, FileError::InvalidPath(_)),
                "{raw:?} 应被判为保留名, 实际 {err:?}"
            );
        }
        // 带目录的保留名同样拒绝。
        assert!(sanitize_relative_path("dir/aux.txt").is_err());
        // 仅前缀相似不误伤。
        assert!(sanitize_relative_path("auxiliary.txt").is_ok());
        assert!(sanitize_relative_path("com10.txt").is_ok());
    }

    #[test]
    fn rejects_bad_chars_and_trailing_dot_space() {
        for raw in ["a:b", "file.txt:stream", "a?b", "a*b", "a<b", "a|b", "a\"b"] {
            assert!(sanitize_relative_path(raw).is_err(), "{raw:?} 应被拒绝");
        }
        assert!(sanitize_relative_path("trailing.").is_err());
        assert!(sanitize_relative_path("trailing ").is_err());
        // 前导空格是合法文件名。
        assert!(sanitize_relative_path(" leading.txt").is_ok());
    }

    #[test]
    fn file_name_has_no_separators() {
        assert_eq!(sanitize_file_name("报告.pdf").unwrap(), "报告.pdf");
        assert!(sanitize_file_name("a/b").is_err());
        assert!(sanitize_file_name(r"a\b").is_err());
        assert!(sanitize_file_name("aux").is_err());
    }

    #[test]
    fn resolve_within_rejects_parent_escape() {
        let root = Path::new("D:\\recv-root");
        // resolve_within 只接受净化后的相对路径；此处直接构造含 .. 的输入会被词法检查兜住。
        let err = resolve_within(root, "../evil").unwrap_err();
        assert!(matches!(err, FileError::InvalidPath(_)));
    }
}
