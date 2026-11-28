// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// 大肥鱼远程 —— 平台剪贴板后端与拉取式同步状态机。

//! 剪贴板后端：Windows Win32 与 Linux X11 CLIPBOARD。
//!
//! [`create_backend`] 不启动传输，也不读取内容。调用方必须先检查每个连接的
//! 授权、同步方向与手动/自动策略，再调用格式枚举、按需读取或同步状态机。
//! Windows 使用 WM_CLIPBOARDUPDATE；Linux 通过 XFixes 监听所有权变更，
//! `X11Clipboard::subscribe_changes` 只报告变化，不读取载荷。
//!
//! [`ClipboardBackend::list_formats`] 返回 [`FormatList`] 元数据；
//! [`ClipboardBackend::read_block`] 才按调用方要求获取载荷。X11 的 TARGETS
//! 不包含长度，因此即使 `max == 0`，大小探测也需要一次有界转换并缓存内容。
//! 写入后用同一后端的 [`ClipboardBackend::sequence`] 更新
//! [`LoopGuard::set_last_written_seq`]，避免跨平台回环。
//!
//! Windows 读取 CF_DIB/CF_DIBV5 会转换成 PNG（见 [`dib_to_png`]）；
//! Linux 通过 UTF8_STRING/text/plain、text/html、image/png 互操作，
//! 原生格式 ID 是 X Atom，清单名使用协议标准名以兼容同步状态机。
//!
//! Windows 操作采用 OpenClipboard/CloseClipboard RAII。Linux 使用独立线程
//! 串行服务选择请求，后端存活时保持所有权；丢弃后端或退出进程后不再保留内容。
//! INCR 收发、内存与等待均有上限。原生 Wayland 需 compositor 授权，目前明确报错。

use std::fmt;
use std::path::PathBuf;

use anyhow::Result;

mod dib;
mod format;
mod hdrop;
mod html;
mod sync;
#[cfg(windows)]
mod r#win;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod stub;
#[cfg(any(target_os = "linux", test))]
mod x11_state;

pub use dib::dib_to_png;
pub use format::{kind_for_format, predefined_format_name, ClipFormat, ClipKind, FormatList};
pub use hdrop::{encode_file_list, parse_file_list};
pub use html::{extract_html_fragment, parse_html_offsets, HtmlOffsets, HTML_FORMAT_NAME};
#[cfg(target_os = "linux")]
pub use linux::{X11Clipboard, X11ClipboardOptions, DEFAULT_X11_MAX_BYTES};
#[cfg(windows)]
pub use r#win::{read_all, watch, ClipboardWatcher};
pub use sync::{
    hash_bytes, kind_from_name, message_from_envelope, message_from_proto_list,
    message_to_envelope, ClipFormatItem, ClipboardSync, SyncMessage, CHUNK_BYTES, MAX_SYNC_BYTES,
};

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
    /// 当前剪贴板序列号。Win32 使用 `GetClipboardSequenceNumber`；
    /// X11 使用本后端监听到的所有权变更代数，只能在同一实例内比较。
    ///
    /// 所有者应在替换内容时重新声明 X11 所有权，后端据此递增序列号。
    fn sequence(&self) -> Result<u64>;

    /// 拉取式格式枚举：当前支持的格式 + 序列号快照。
    ///
    /// Windows 保持 `EnumClipboardFormats` 顺序；X11 按文本、HTML、PNG 返回
    /// 每类首选目标。调用方按自己的策略选择，不应依赖跨平台格式顺序。
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

/// 创建当前平台的后端。Windows 使用 Win32，Linux 使用 X11 CLIPBOARD。
///
/// 创建后端不读取载荷、不启动同步。调用方必须先检查会话授权与方向策略。
/// Linux 原生 Wayland 会话明确报错；不能以 DISPLAY/XWayland 代替 compositor 授权。
pub fn create_backend() -> Result<Box<dyn ClipboardBackend>> {
    #[cfg(windows)]
    {
        r#win::create_backend()
    }
    #[cfg(target_os = "linux")]
    {
        linux::create_backend()
    }
    #[cfg(target_os = "macos")]
    {
        macos::create_backend()
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
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
        #[cfg(target_os = "macos")]
        {
            let seq = macos::current_change_count();
            if seq > self.last_written_seq {
                self.last_written_seq = seq;
            }
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            // 非 Windows/macOS 无全局序列号可查；保留旧值，调用方可用 set_last_written_seq 显式设置。
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
