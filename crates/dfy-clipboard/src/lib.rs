// SPDX-License-Identifier: AGPL-3.0-or-later
// 大肥鱼远程 —— dfy-clipboard：Windows 剪贴板后端 + 变更监听 + 拉取式格式枚举。
// 控制端与被控端共用本 crate 的抽象；macOS/Linux 以后按同一个 ClipboardBackend trait 补齐。

//! 剪贴板同步模块（Win32 后端）。
//!
//! 机制照抄 UU 远控的“事件驱动 + 拉模式”，而不是整包推送：
//!
//! 1. 被控端用 [`watch`] 监听 `WM_CLIPBOARDUPDATE`，只把 [`FormatList`]（格式清单 + 序列号）发给对端；
//! 2. 对端按需调用 [`ClipboardBackend::read_block`]，按 `offset`/`max` 分块拉取（建议 32–256 KiB/块），
//!    天然支持大对象限速与断点续传；
//! 3. 写回后调用 [`LoopGuard::note_write`] 记下写入后的剪贴板序列号，在监听回调里用
//!    [`LoopGuard::should_ignore`] 过滤掉自己写入触发的事件，避免两端互相覆盖形成回环。
//!
//! # 图像格式约定（重要）
//!
//! * 读取时，`CF_DIB`/`CF_DIBV5` 一律**转换成 PNG** 再返回（见 [`dib_to_png`]），
//!   对端拿到的 `kind == ClipKind::Dib` 的数据块内容其实是 PNG 字节流；
//! * 写入时 `ClipKind::Png` 写入注册格式 `"PNG"`，`ClipKind::Dib` 则按原样写入 `CF_DIB`。
//!   也就是说“跨端传输统一 PNG、本机落盘按目标格式各自转换”。
//! * 支持 8/16/24/32bpp 的 `BI_RGB` 与 `BI_BITFIELDS` DIB；1/4bpp 及 `CF_BITMAP`（HBITMAP）不支持。
//!
//! # 并发
//!
//! 剪贴板是窗口站/桌面级全局资源。后端每次操作都独立 `OpenClipboard` → 操作 → `CloseClipboard`
//! （RAII 守卫，异常路径也不会锁死剪贴板），打开失败最多重试 5 次、每次间隔 20ms。
//! 后端本身不持有内部锁：**不可并发写入**，写入方必须自行串行化；读取可以并发但会被目标进程占用时重试。

use std::fmt;
use std::path::PathBuf;

use anyhow::Result;

mod dib;
mod format;
#[cfg(windows)]
mod hdrop;
mod html;
#[cfg(windows)]
mod r#win;

#[cfg(not(windows))]
mod stub;

pub use dib::dib_to_png;
pub use format::{kind_for_format, predefined_format_name, ClipFormat, ClipKind, FormatList};
#[cfg(windows)]
pub use hdrop::{encode_file_list, parse_file_list};
pub use html::{extract_html_fragment, parse_html_offsets, HtmlOffsets, HTML_FORMAT_NAME};
#[cfg(windows)]
pub use r#win::{read_all, watch, ClipboardWatcher};

/// 一次拉取请求返回的数据块。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataBlock {
    /// 剪贴板格式 ID（与 [`ClipFormat::id`] 一致）。
    pub id: u32,
    /// 该格式（转换后）数据的总长度，单位字节。
    pub total_len: u64,
    /// 本块的起始偏移。
    pub offset: u64,
    /// 本块数据；`offset >= total_len` 时为空（不视为错误，方便对端处理对象变更）。
    pub block: Vec<u8>,
}

/// 平台无关的剪贴板后端抽象（被控端读取、控制端写回共用）。
///
/// 实现要求 `Send`，便于放进后台线程；但剪贴板是全局资源，写入必须由调用方串行化。
pub trait ClipboardBackend: Send {
    /// 当前剪贴板序列号（Win32: `GetClipboardSequenceNumber`）。
    ///
    /// 序列号只要剪贴板内容改变就会递增，是判断“是否真的变了”的廉价依据。
    fn sequence(&self) -> Result<u64>;

    /// 拉取式格式枚举：当前剪贴板所有格式 + 序列号快照。
    ///
    /// 返回的格式顺序与 `EnumClipboardFormats` 一致（系统格式在前、注册格式在后），
    /// 调用方按自己的优先级选择（建议 文本 > 图片 > 文件列表）。
    fn list_formats(&self) -> Result<FormatList>;

    /// 按偏移拉取某个格式的数据块。
    ///
    /// * `format_id`：来自 [`ClipFormat::id`]；
    /// * `offset`：起始字节偏移；
    /// * `max`：最多返回的字节数（0 表示只取 `total_len`，方便探测大小）。
    ///
    /// 文本类返回 UTF-8；DIB 返回 PNG；HTML 返回 fragment 片段；其余返回原始字节。
    fn read_block(&self, format_id: u32, offset: u64, max: usize) -> Result<DataBlock>;

    /// 写入剪贴板（先清空再写单个格式；文件列表用 `files` 构造 `CF_HDROP`）。
    ///
    /// `bytes` 对 `Text` 是 UTF-8、对 `Html` 是 HTML 片段、对 `Png` 是 PNG 文件字节、
    /// 对 `Dib` 是原始 DIB、对 `Rtf` 是 RTF 字节、对 `Other(name)` 是原始字节。
    fn write(&mut self, kind: &ClipKind, bytes: &[u8], files: &[PathBuf]) -> Result<()>;
}

/// 创建当前平台的后端。Windows 上始终可用；其它平台返回“不支持”错误。
pub fn create_backend() -> Result<Box<dyn ClipboardBackend>> {
    #[cfg(windows)]
    {
        r#win::create_backend()
    }
    #[cfg(not(windows))]
    {
        stub::create_backend()
    }
}

/// 自激抑制守卫：记住“自己刚写入之后”的剪贴板序列号，用来过滤回环事件。
///
/// 典型用法：
/// ```no_run
/// # use dfy_clipboard::LoopGuard;
/// let mut guard = LoopGuard::new();
/// // 写入剪贴板之后：
/// guard.note_write();
/// // 监听回调里：
/// // if guard.should_ignore(seq) { return; }
/// # let _ = &guard;
/// ```
#[derive(Debug, Clone, Default)]
pub struct LoopGuard {
    /// 自己最后一次写入完成时观察到的剪贴板序列号；0 表示还没写过。
    last_written_seq: u64,
}

impl LoopGuard {
    /// 新建守卫（尚未写过，`last_written_seq == 0`）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 写入剪贴板成功后调用：记录当前序列号作为“自己造成的更新”下界。
    ///
    /// 序列号单调递增且可能一次跳到更大的值（系统内部多次更新），所以取当前值即可；
    /// 早于/等于该值的通知都视为自己写入的回波。
    pub fn note_write(&mut self) {
        #[cfg(windows)]
        {
            let seq = unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() }
                as u64;
            if seq > self.last_written_seq {
                self.last_written_seq = seq;
            }
        }
        #[cfg(not(windows))]
        {
            // 非 Windows 无序列号可查；保留旧值，调用方可用 with_last_written_seq 显式设置。
        }
    }

    /// 自己最后一次写入后观察到的序列号（0 = 尚未写入）。
    pub fn last_written_seq(&self) -> u64 {
        self.last_written_seq
    }

    /// 是否应忽略该序列号对应的变更通知（即判断为“自己刚写的那次”）。
    ///
    /// 注意：`seq <= last_written_seq` 全部忽略；对端如果在更晚时刻又改了剪贴板，
    /// 序列号会继续增长，自然会越过该下界，不会被误吞。
    pub fn should_ignore(&self, seq: u64) -> bool {
        if self.last_written_seq == 0 {
            seq == 0
        } else {
            seq <= self.last_written_seq
        }
    }

    /// 测试/非 Windows 场景下显式设置下界。
    pub fn set_last_written_seq(&mut self, seq: u64) {
        self.last_written_seq = seq;
    }
}

impl fmt::Display for DataBlock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "DataBlock(format={}, offset={}, len={}/{})",
            self.id,
            self.offset,
            self.block.len(),
            self.total_len
        )
    }
}
