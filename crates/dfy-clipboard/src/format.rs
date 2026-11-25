// SPDX-License-Identifier: AGPL-3.0-or-later
// 大肥鱼远程 —— 剪贴板格式类型、系统格式名表与 kind 归类规则。
// 这里全部是平台无关逻辑，便于控制端/被控端共用和单元测试。

use std::fmt;

/// 统一的剪贴板数据种类；跨端协议只同步这些种类（其余按 [`ClipKind::Other`] 透传）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ClipKind {
    /// 纯文本（`CF_UNICODETEXT`；`CF_TEXT`/`CF_OEMTEXT` 也归到这里）。
    Text,
    /// HTML 片段（注册格式 `"HTML Format"`，即 CF_HTML）。
    Html,
    /// PNG 图片（注册格式 `"PNG"` / `"image/png"`）。
    Png,
    /// 设备无关位图（`CF_DIB` / `CF_DIBV5`）。读取时会转成 PNG。
    Dib,
    /// 文件列表（`CF_HDROP`，复制文件到对端）。
    FileList,
    /// 富文本（注册格式 `"Rich Text Format"`）。
    Rtf,
    /// 其它未识别格式，携带注册名。
    Other(String),
}

impl ClipKind {
    /// 是否为文本类（含 OEM 文本降级）。
    pub fn is_text(&self) -> bool {
        matches!(self, ClipKind::Text)
    }
}

impl fmt::Display for ClipKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClipKind::Text => f.write_str("text"),
            ClipKind::Html => f.write_str("html"),
            ClipKind::Png => f.write_str("png"),
            ClipKind::Dib => f.write_str("dib"),
            ClipKind::FileList => f.write_str("file-list"),
            ClipKind::Rtf => f.write_str("rtf"),
            ClipKind::Other(name) => write!(f, "other({name})"),
        }
    }
}

/// 一个可用剪贴板格式的描述。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipFormat {
    /// Win32 格式 ID（系统格式是 `CF_*` 常量，注册格式是运行时分配的 0xC000+ 值）。
    pub id: u32,
    /// 人类可读名字：系统格式为 `CF_*` 名，注册格式为注册名（如 `"HTML Format"`）。
    pub name: String,
    /// 归类后的数据种类。
    pub kind: ClipKind,
}

/// 拉取式格式清单快照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatList {
    /// 快照对应的剪贴板序列号。
    pub sequence: u64,
    /// 当前所有格式，顺序与 `EnumClipboardFormats` 一致。
    pub formats: Vec<ClipFormat>,
}

impl FormatList {
    /// 按格式 ID 查找。
    pub fn find(&self, id: u32) -> Option<&ClipFormat> {
        self.formats.iter().find(|f| f.id == id)
    }

    /// 是否存在某一类数据。
    pub fn contains_kind(&self, kind: &ClipKind) -> bool {
        self.formats.iter().any(|f| &f.kind == kind)
    }

    /// 按调用方优先级挑第一个匹配的格式（UU 建议：文本 > 图片 > 文件列表）。
    pub fn first_of(&self, priority: &[ClipKind]) -> Option<&ClipFormat> {
        for want in priority {
            if let Some(hit) = self.formats.iter().find(|f| &f.kind == want) {
                return Some(hit);
            }
        }
        None
    }
}

/// 系统预定义格式（`id < 0xC000`）的名字表；未列出的返回 `None`。
pub fn predefined_format_name(id: u32) -> Option<&'static str> {
    Some(match id {
        1 => "CF_TEXT",
        2 => "CF_BITMAP",
        3 => "CF_METAFILEPICT",
        4 => "CF_SYLK",
        5 => "CF_DIF",
        6 => "CF_TIFF",
        7 => "CF_OEMTEXT",
        8 => "CF_DIB",
        9 => "CF_PALETTE",
        10 => "CF_PENDATA",
        11 => "CF_RIFF",
        12 => "CF_WAVE",
        13 => "CF_UNICODETEXT",
        14 => "CF_ENHMETAFILE",
        15 => "CF_HDROP",
        16 => "CF_LOCALE",
        17 => "CF_DIBV5",
        _ => return None,
    })
}

/// 常见注册格式名（大小写不敏感比较）对应到内部种类。
///
/// * `"HTML Format"` —— Chrome/Edge/Word 等的 CF_HTML；
/// * `"PNG"` / `"image/png"` —— Chromium 系浏览器的 PNG 图像；
/// * `"Rich Text Format"` —— WordPad/Word 的 RTF。
pub fn kind_for_format(id: u32, name: &str) -> ClipKind {
    match id {
        1 | 7 | 13 => return ClipKind::Text,
        8 | 17 => return ClipKind::Dib,
        15 => return ClipKind::FileList,
        _ => {}
    }
    let lowered = name.trim().to_ascii_lowercase();
    match lowered.as_str() {
        "html format" => ClipKind::Html,
        "rich text format" => ClipKind::Rtf,
        "png" | "image/png" | "png image" => ClipKind::Png,
        _ => ClipKind::Other(name.to_string()),
    }
}
