// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 目录递归展开：把一棵目录树变成**确定序**的 `(相对路径, 大小, 是否目录, mtime)` 清单。
//!
//! 发送端先发目录 offer（`is_dir=true`），再逐个 `FileOffer` 传输文件；
//! 空目录同样入清单（`is_dir=true, size=0`），保证接收端能还原空目录。
//!
//! 默认跳过符号链接（防止把链接目标或环带进传输）与特殊文件；
//! 可选跳过系统隐藏文件（Windows 看 FILE_ATTRIBUTE_HIDDEN/SYSTEM，
//! 其它平台按 Unix 惯例的 `.` 前缀）。

use std::fs::{self, DirEntry, Metadata};
use std::path::Path;
use std::time::UNIX_EPOCH;

use crate::path::sanitize_file_name;
use crate::FileError;

/// 目录展开的最大深度（防御恶意/异常深的树导致栈或遍历爆炸）。
const MAX_DEPTH: usize = 128;

/// 目录树中的一项（相对路径以 `/` 分隔，已净化）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntryPlan {
    /// 相对于被展开目录的路径（例如 `sub/文件.txt`，已净化）。
    pub relative: String,
    /// 文件字节数（目录为 0）。
    pub size: u64,
    /// 是否目录（空目录也会出现在清单里）。
    pub is_dir: bool,
    /// 修改时间（Unix 毫秒，0=未知）。
    pub last_modified_ms: u64,
}

/// [`plan_directory_with`] 的可选项。
#[derive(Clone, Copy, Debug, Default)]
pub struct PlanOptions {
    /// 跳过系统隐藏文件/目录（Windows 隐藏+系统属性；其它平台 `.` 前缀）。
    pub skip_hidden: bool,
}

/// 递归展开目录（默认包含隐藏文件、跳过符号链接），结果按相对路径**字节序**排序。
///
/// 排序保证：父目录一定排在子项之前（`sub` < `sub/x`），两次调用结果一致，
/// 便于测试与断点续传时对齐。
pub fn plan_directory(path: &Path) -> Result<Vec<DirEntryPlan>, FileError> {
    plan_directory_with(path, PlanOptions::default())
}

/// 带可选项的目录展开，见 [`PlanOptions`]。
///
/// 错误：根不是目录、文件名非 UTF-8/无法净化、读目录或元数据失败。
pub fn plan_directory_with(path: &Path, opts: PlanOptions) -> Result<Vec<DirEntryPlan>, FileError> {
    let root_md = fs::metadata(path).map_err(|err| FileError::io(path, err))?;
    if !root_md.is_dir() {
        return Err(FileError::invalid(format!(
            "plan_directory 需要目录: {}",
            path.display()
        )));
    }

    let mut entries = Vec::new();
    // 显式栈迭代，避免深目录递归爆栈。
    let mut stack: Vec<(std::path::PathBuf, String, usize)> =
        vec![(path.to_path_buf(), String::new(), 0)];

    while let Some((dir, prefix, depth)) = stack.pop() {
        if depth > MAX_DEPTH {
            return Err(FileError::invalid(format!(
                "目录层级超过 {MAX_DEPTH}: {}",
                dir.display()
            )));
        }
        let children = read_sorted_children(&dir)?;
        for child in children {
            let file_name = child.file_name();
            let name = file_name.to_str().ok_or_else(|| {
                FileError::invalid(format!(
                    "文件名不是 UTF-8, 无法跨端传输: {}",
                    child.path().display()
                ))
            })?;
            let name = sanitize_file_name(name)?;
            let child_path = child.path();
            // `file_type()` 不跟随符号链接：链接一律跳过（不传输链接目标，也不跟随目录链接成环）。
            let file_type = child
                .file_type()
                .map_err(|err| FileError::io(&child_path, err))?;
            if file_type.is_symlink() {
                tracing::debug!(path = %child_path.display(), "跳过符号链接");
                continue;
            }
            let md = child
                .metadata()
                .map_err(|err| FileError::io(&child_path, err))?;
            if opts.skip_hidden && is_hidden(&child, &md) {
                tracing::debug!(path = %child_path.display(), "跳过隐藏文件");
                continue;
            }
            let relative = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            if md.is_dir() {
                entries.push(DirEntryPlan {
                    relative: relative.clone(),
                    size: 0,
                    is_dir: true,
                    last_modified_ms: mtime_ms(&md),
                });
                stack.push((child_path, relative, depth + 1));
            } else if md.is_file() {
                entries.push(DirEntryPlan {
                    relative,
                    size: md.len(),
                    is_dir: false,
                    last_modified_ms: mtime_ms(&md),
                });
            } else {
                // 管道/socket/设备等特殊文件不参与跨端传输。
                tracing::debug!(path = %child_path.display(), "跳过特殊文件");
            }
        }
    }

    // 字节序排序：确定性，且父目录（前缀）一定在子项之前。
    entries.sort_by(|a, b| a.relative.cmp(&b.relative));
    Ok(entries)
}

/// 读取目录并按文件名排序（`read_dir` 本身顺序未定义，这里先稳定排序）。
fn read_sorted_children(dir: &Path) -> Result<Vec<DirEntry>, FileError> {
    let mut children = Vec::new();
    for entry in fs::read_dir(dir).map_err(|err| FileError::io(dir, err))? {
        children.push(entry.map_err(|err| FileError::io(dir, err))?);
    }
    children.sort_by_key(|a| a.file_name());
    Ok(children)
}

/// 文件修改时间 → Unix 毫秒（0=未知/早于纪元）。
pub(crate) fn mtime_ms(md: &Metadata) -> u64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

/// Windows：FILE_ATTRIBUTE_HIDDEN(0x2) 或 FILE_ATTRIBUTE_SYSTEM(0x4)。
#[cfg(windows)]
fn is_hidden(entry: &DirEntry, md: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x0000_0002;
    const FILE_ATTRIBUTE_SYSTEM: u32 = 0x0000_0004;
    if md.file_attributes() & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM) != 0 {
        return true;
    }
    // dotfile 在 Windows 上不是隐藏属性，但用户常这么用；一并视为隐藏。
    entry.file_name().to_string_lossy().starts_with('.')
}

/// 非 Windows：按 Unix 惯例的 `.` 前缀判断。
#[cfg(not(windows))]
fn is_hidden(entry: &DirEntry, _md: &Metadata) -> bool {
    entry.file_name().to_string_lossy().starts_with('.')
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    fn build_tree(root: &Path) {
        fs::write(root.join("a.txt"), b"a").unwrap();
        fs::create_dir_all(root.join("empty")).unwrap();
        fs::create_dir_all(root.join("sub/deeper")).unwrap();
        fs::write(root.join("sub/b.bin"), vec![7u8; 33]).unwrap();
        fs::write(root.join("sub/deeper/c.txt"), b"ccc").unwrap();
        fs::create_dir_all(root.join("中文")).unwrap();
        fs::write(root.join("中文/文件.txt"), "你好".as_bytes()).unwrap();
        fs::write(root.join(".hidden"), b"secret").unwrap();
    }

    #[test]
    fn plans_three_levels_with_empty_dirs_and_chinese_names() {
        let tmp = tempdir().unwrap();
        build_tree(tmp.path());

        let plan = plan_directory(tmp.path()).unwrap();
        let relatives: Vec<&str> = plan.iter().map(|e| e.relative.as_str()).collect();
        assert_eq!(
            relatives,
            vec![
                ".hidden",
                "a.txt",
                "empty",
                "sub",
                "sub/b.bin",
                "sub/deeper",
                "sub/deeper/c.txt",
                "中文",
                "中文/文件.txt",
            ]
        );

        let empty = plan.iter().find(|e| e.relative == "empty").unwrap();
        assert!(empty.is_dir);
        assert_eq!(empty.size, 0);

        let chinese = plan.iter().find(|e| e.relative == "中文/文件.txt").unwrap();
        assert!(!chinese.is_dir);
        assert_eq!(chinese.size, "你好".len() as u64);

        // 确定性：两次展开的相对路径/大小/类型完全一致。
        // （mtime 是文件系统元数据，不参与确定性断言：NTFS 目录 mtime 偶有毫秒级抖动。）
        let projection = |plan: &[DirEntryPlan]| -> Vec<(String, u64, bool)> {
            plan.iter()
                .map(|e| (e.relative.clone(), e.size, e.is_dir))
                .collect()
        };
        assert_eq!(
            projection(&plan),
            projection(&plan_directory(tmp.path()).unwrap())
        );
    }

    #[test]
    fn skip_hidden_option() {
        let tmp = tempdir().unwrap();
        build_tree(tmp.path());
        let plan = plan_directory_with(tmp.path(), PlanOptions { skip_hidden: true }).unwrap();
        assert!(!plan.iter().any(|e| e.relative == ".hidden"));
    }

    #[test]
    fn rejects_non_directory_and_bad_names() {
        let tmp = tempdir().unwrap();
        let file = tmp.path().join("plain.txt");
        fs::write(&file, b"x").unwrap();
        let err = plan_directory(&file).unwrap_err();
        assert!(matches!(err, FileError::InvalidPath(_)));

        // Linux 上允许创建 `aux.txt`，展开时必须拒绝（Windows 接收端会当成设备名）。
        #[cfg(not(windows))]
        {
            let dir = tmp.path().join("d");
            fs::create_dir(&dir).unwrap();
            fs::write(dir.join("aux.txt"), b"x").unwrap();
            assert!(plan_directory(&dir).is_err());
        }
    }

    #[test]
    fn skips_symlinks_when_supported() {
        let tmp = tempdir().unwrap();
        build_tree(tmp.path());
        let link = tmp.path().join("link-to-sub");
        #[cfg(windows)]
        let created = std::os::windows::fs::symlink_dir(tmp.path().join("sub"), &link).is_ok();
        #[cfg(not(windows))]
        let created = std::os::unix::fs::symlink(tmp.path().join("sub"), &link).is_ok();
        // Windows 未开开发者模式时普通用户无法建符号链接：跳过断言，不算失败。
        if created {
            let plan = plan_directory(tmp.path()).unwrap();
            assert!(!plan.iter().any(|e| e.relative == "link-to-sub"));
            assert!(plan.iter().any(|e| e.relative == "sub/b.bin"));
        }
    }
}
