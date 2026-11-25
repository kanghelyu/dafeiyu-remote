// SPDX-License-Identifier: AGPL-3.0-or-later
// 大肥鱼远程 —— Win32 剪贴板后端 + `WM_CLIPBOARDUPDATE` 变更监听。
//
// 两种使用方式：
// * [`create_backend`] 拿一个 `ClipboardBackend`，被控端读格式清单/拉块、控制端写回；
// * [`watch`] 起一个后台线程 + 消息窗口，剪贴板变化时把序列号回调出来（事件驱动）。
//
// 并发纪律：剪贴板是全局资源，`write` 不允许并发调用（调用方自行串行化）；
// 读取会重试 5 次（每次 20ms 间隔），避开其它进程占用剪贴板的短暂窗口。

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{anyhow, bail, ensure, Context, Result};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, EnumClipboardFormats,
    GetClipboardData, GetClipboardFormatNameW, GetClipboardSequenceNumber, OpenClipboard,
    RegisterClipboardFormatW, RemoveClipboardFormatListener, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, PostThreadMessageW,
    RegisterClassW, TranslateMessage, HWND_MESSAGE, MSG, WINDOW_EX_STYLE, WINDOW_STYLE,
    WM_CLIPBOARDUPDATE, WM_QUIT, WNDCLASSW,
};

use crate::dib::dib_to_png;
use crate::format::{kind_for_format, predefined_format_name, ClipFormat, ClipKind, FormatList};
use crate::hdrop::encode_file_list;
use crate::html::{extract_html_fragment, wrap_html_fragment, HTML_FORMAT_NAME};
use crate::{ClipboardBackend, DataBlock};

/// 系统预定义格式 ID。
const CF_TEXT: u32 = 1;
const CF_DIB: u32 = 8;
const CF_UNICODETEXT: u32 = 13;
const CF_HDROP: u32 = 15;
const CF_DIBV5: u32 = 17;

/// `OpenClipboard` 的 RAII 守卫：无论成功失败，离开作用域都会 `CloseClipboard`。
struct ClipboardGuard;

impl ClipboardGuard {
    /// 打开剪贴板；被其它进程占用时最多重试 5 次（20ms 间隔）。
    fn open() -> Result<Self> {
        let mut last = None;
        for attempt in 0..5 {
            // SAFETY: 传 NULL 表示把剪贴板关联到当前任务；失败仅返回 Err，不会产生悬垂句柄。
            match unsafe { OpenClipboard(None) } {
                Ok(()) => return Ok(Self),
                Err(err) => {
                    last = Some(err);
                    if attempt < 4 {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }
            }
        }
        Err(anyhow!(
            "打开剪贴板失败（重试 5 次）：{}",
            last.map(|e| e.to_string()).unwrap_or_default()
        ))
    }
}

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        // SAFETY: 只有 Guard 存活时才持有剪贴板所有权，因此这里必然配平。
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

/// 把一段字节复制进可移动全局内存块，返回交给剪贴板的句柄（所有权随 `SetClipboardData` 转移）。
fn global_copy(bytes: &[u8]) -> Result<HANDLE> {
    // SAFETY: GlobalAlloc/GlobalLock 按 Windows 约定使用；失败返回 null，均做了判空。
    unsafe {
        let hglobal: HGLOBAL = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1))
            .map_err(|e| anyhow!("GlobalAlloc 失败：{e}"))?;
        let ptr = GlobalLock(hglobal);
        ensure!(!ptr.is_null(), "GlobalLock 失败");
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
        let _ = GlobalUnlock(hglobal);
        Ok(HANDLE(hglobal.0))
    }
}

/// 从剪贴板句柄读出全部字节（`GlobalSize` 决定长度）。
fn global_bytes(handle: HANDLE) -> Result<Vec<u8>> {
    // SAFETY: 句柄由 GetClipboardData 返回，处于打开状态期间有效。
    unsafe {
        let hglobal = HGLOBAL(handle.0);
        let size = GlobalSize(hglobal);
        // 注意：全局内存可能因页对齐而大于实际数据；剪贴板文本的 NUL 结尾在数据内，
        // 调用方需要按格式自行截断（文本读取见 read_unicode_text）。
        let ptr = GlobalLock(hglobal);
        ensure!(!ptr.is_null(), "GlobalLock 失败");
        let mut out = vec![0u8; size];
        std::ptr::copy_nonoverlapping(ptr as *const u8, out.as_mut_ptr(), size);
        let _ = GlobalUnlock(hglobal);
        Ok(out)
    }
}

/// 读取 `CF_UNICODETEXT` 并转为 UTF-8 字节。
fn read_unicode_text(handle: HANDLE) -> Result<Vec<u8>> {
    // SAFETY: 句柄来自 GetClipboardData(CF_UNICODETEXT)，内容是以 NUL 结尾的 UTF-16。
    unsafe {
        let hglobal = HGLOBAL(handle.0);
        let ptr = GlobalLock(hglobal);
        ensure!(!ptr.is_null(), "GlobalLock 失败");
        let mut units: Vec<u16> = Vec::new();
        let mut i = 0usize;
        loop {
            let unit = std::ptr::read_unaligned((ptr as *const u16).add(i));
            if unit == 0 {
                break;
            }
            units.push(unit);
            i += 1;
            ensure!(i < 64 * 1024 * 1024, "剪贴板文本异常（缺少 NUL 结尾）");
        }
        let _ = GlobalUnlock(hglobal);
        Ok(String::from_utf16_lossy(&units).into_bytes())
    }
}

/// 注册格式名 → 格式 ID（不存在则注册）。
fn register_format(name: &str) -> Result<u32> {
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: 传入以 NUL 结尾的宽字符串指针；返回值 0 表示失败。
    let id = unsafe { RegisterClipboardFormatW(PCWSTR(wide.as_ptr())) };
    ensure!(id != 0, "注册剪贴板格式失败：{name}");
    Ok(id)
}

/// 格式 ID → 注册名（系统格式返回 `None`）。
fn format_name(id: u32) -> Result<String> {
    if let Some(name) = predefined_format_name(id) {
        return Ok(name.to_string());
    }
    // SAFETY: 缓冲区足够大；返回 0 表示失败（未知格式，按编号兜底）。
    unsafe {
        let mut buf = [0u16; 256];
        let len = GetClipboardFormatNameW(id, &mut buf);
        if len <= 0 {
            Ok(format!("0x{id:04X}"))
        } else {
            Ok(String::from_utf16_lossy(&buf[..len as usize]))
        }
    }
}

/// Win32 剪贴板后端。
#[derive(Debug, Default)]
pub struct WinClipboard;

impl WinClipboard {
    /// 新建后端（无状态）。
    pub fn new() -> Self {
        Self
    }
}

impl ClipboardBackend for WinClipboard {
    fn sequence(&self) -> Result<u64> {
        // SAFETY: 纯查询，无参数无副作用。
        Ok(u64::from(unsafe { GetClipboardSequenceNumber() }))
    }

    fn list_formats(&self) -> Result<FormatList> {
        let guard = ClipboardGuard::open()?;
        let sequence = u64::from(unsafe { GetClipboardSequenceNumber() });
        let mut formats = Vec::new();
        // SAFETY: EnumClipboardFormats 在打开剪贴板期间调用，链式枚举直到返回 0。
        let mut next = unsafe { EnumClipboardFormats(0) };
        let mut guard_count = 0;
        while next != 0 {
            let name = format_name(next)?;
            let kind = kind_for_format(next, &name);
            formats.push(ClipFormat {
                id: next,
                name,
                kind,
            });
            next = unsafe { EnumClipboardFormats(next) };
            guard_count += 1;
            ensure!(guard_count < 512, "枚举剪贴板格式异常（超过 512 项）");
        }
        drop(guard);
        Ok(FormatList { sequence, formats })
    }

    fn read_block(&self, format_id: u32, offset: u64, max: usize) -> Result<DataBlock> {
        let guard = ClipboardGuard::open()?;
        // SAFETY: GetClipboardData 在打开剪贴板期间调用，句柄随 CloseClipboard 失效，
        // 所以所有读取都必须在本函数内完成（Guard 之后才 drop）。
        let handle = unsafe { GetClipboardData(format_id) }
            .with_context(|| format!("剪贴板中没有格式 {format_id}（可能已被其它程序改写）"))?;
        ensure!(!handle.is_invalid(), "剪贴板格式 {format_id} 返回了空句柄");

        let converted: Vec<u8> = match format_id {
            CF_UNICODETEXT | CF_TEXT => read_unicode_text(handle)?,
            CF_DIB | CF_DIBV5 => {
                let raw = global_bytes(handle)?;
                dib_to_png(&raw).context("DIB 转 PNG 失败")?
            }
            CF_HDROP => global_bytes(handle)?,
            _ => {
                let raw = global_bytes(handle)?;
                // HTML 片段只保留 fragment 部分（CF_HTML 的头部偏移由调用方无需关心）
                let name = format_name(format_id)?;
                if name.eq_ignore_ascii_case(HTML_FORMAT_NAME) {
                    extract_html_fragment(&raw).to_vec()
                } else {
                    raw
                }
            }
        };
        drop(guard);

        let total_len = converted.len() as u64;
        let start = offset.min(total_len) as usize;
        let end = if max == 0 {
            start
        } else {
            (start + max).min(converted.len())
        };
        Ok(DataBlock {
            id: format_id,
            total_len,
            offset,
            block: converted[start..end].to_vec(),
        })
    }

    fn write(&mut self, kind: &ClipKind, bytes: &[u8], files: &[PathBuf]) -> Result<()> {
        let guard = ClipboardGuard::open()?;
        // SAFETY: 清空与写入都要求已打开剪贴板。
        unsafe { EmptyClipboard() }.map_err(|e| anyhow!("清空剪贴板失败：{e}"))?;

        let (format_id, payload): (u32, Vec<u8>) = match kind {
            ClipKind::Text => {
                let text = std::str::from_utf8(bytes).context("文本剪贴板内容不是 UTF-8")?;
                let mut wide: Vec<u16> = text.encode_utf16().collect();
                wide.push(0);
                let mut buffer = Vec::with_capacity(wide.len() * 2);
                for unit in wide {
                    buffer.extend_from_slice(&unit.to_le_bytes());
                }
                (CF_UNICODETEXT, buffer)
            }
            ClipKind::Html => {
                let wrapped = wrap_html_fragment(bytes).context("包装 CF_HTML 失败")?;
                (register_format(HTML_FORMAT_NAME)?, wrapped)
            }
            ClipKind::Png => (register_format("PNG")?, bytes.to_vec()),
            ClipKind::Dib => (CF_DIB, bytes.to_vec()),
            ClipKind::FileList => (CF_HDROP, encode_file_list(files)),
            ClipKind::Rtf => (register_format("Rich Text Format")?, bytes.to_vec()),
            ClipKind::Other(name) => (register_format(name)?, bytes.to_vec()),
        };

        let handle = global_copy(&payload)?;
        // SAFETY: 句柄所有权在 SetClipboardData 成功后转移给系统；失败时由系统未接管，
        // 这里不再释放会有一次小泄漏，但失败路径极少且进程随后通常会报告错误。
        match unsafe { SetClipboardData(format_id, Some(handle)) } {
            Ok(_) => {
                drop(guard);
                Ok(())
            }
            Err(err) => {
                drop(guard);
                bail!("SetClipboardData 失败（格式 {format_id}）：{err}")
            }
        }
    }
}

/// 创建 Windows 剪贴板后端。
pub fn create_backend() -> Result<Box<dyn ClipboardBackend>> {
    Ok(Box::new(WinClipboard::new()))
}

/// 剪贴板变更监听器：后台线程 + 消息专窗，收到 `WM_CLIPBOARDUPDATE` 时回调序列号。
pub struct ClipboardWatcher {
    thread_id: u32,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl ClipboardWatcher {
    /// 启动监听；`callback` 在监听线程里被调用（不要在其中做长时间阻塞操作）。
    pub fn spawn(callback: impl Fn(u64) + Send + 'static) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let (tx, rx) = std::sync::mpsc::channel::<u32>();

        let join = thread::Builder::new()
            .name("dfy-clipboard-watch".into())
            .spawn(move || {
                let thread_id = unsafe { GetCurrentThreadId() };
                let _ = tx.send(thread_id);
                if let Err(err) = run_message_loop(stop_thread, callback) {
                    tracing::warn!(error = %err, "剪贴板监听线程异常退出");
                }
            })
            .context("启动剪贴板监听线程失败")?;

        let thread_id = rx
            .recv_timeout(Duration::from_secs(5))
            .context("等待剪贴板监听线程就绪超时")?;
        Ok(Self {
            thread_id,
            stop,
            join: Some(join),
        })
    }

    /// 监听线程 ID（调试用）。
    pub fn thread_id(&self) -> u32 {
        self.thread_id
    }

    /// 请求停止：向监听线程投递 `WM_QUIT`，并等待线程退出。
    pub fn stop(&mut self) {
        if self.stop.swap(true, Ordering::SeqCst) {
            return;
        }
        // SAFETY: PostThreadMessageW 向已知线程投递消息；线程可能已退出，失败可忽略。
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(handle) = self.join.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for ClipboardWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 监听线程主体：创建消息专窗、注册剪贴板监听、跑消息循环。
fn run_message_loop(stop: Arc<AtomicBool>, callback: impl Fn(u64) + Send + 'static) -> Result<()> {
    // SAFETY: 标准的 Win32 窗口类注册 + 消息循环流程；类名用宽字符串字面量。
    unsafe {
        let instance = GetModuleHandleW(None).map_err(|e| anyhow!("GetModuleHandleW 失败：{e}"))?;
        let class_name = w!("DfyClipboardWatcherWindow");
        let wnd_class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: class_name,
            ..Default::default()
        };
        let atom = RegisterClassW(&wnd_class);
        ensure!(atom != 0, "RegisterClassW 失败");

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            w!("dfy-clipboard-watcher"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance.into()),
            None,
        )
        .map_err(|e| anyhow!("CreateWindowExW 失败：{e}"))?;

        AddClipboardFormatListener(hwnd)
            .map_err(|e| anyhow!("AddClipboardFormatListener 失败：{e}"))?;
        // 把回调放进窗口用户数据，供窗口过程取用
        let boxed: Box<Box<dyn Fn(u64) + Send>> = Box::new(Box::new(callback));
        let raw = Box::into_raw(boxed);
        windows::Win32::UI::WindowsAndMessaging::SetWindowLongPtrW(
            hwnd,
            windows::Win32::UI::WindowsAndMessaging::GWLP_USERDATA,
            raw as isize,
        );

        let mut msg = MSG::default();
        loop {
            let ret = GetMessageW(&mut msg, None, 0, 0);
            if ret.0 == -1 || ret.0 == 0 || msg.message == WM_QUIT {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
            if stop.load(Ordering::SeqCst) {
                break;
            }
        }

        let _ = RemoveClipboardFormatListener(hwnd);
        // 回收回调 Box（窗口销毁后不再有消息进来）
        let ptr = windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(
            hwnd,
            windows::Win32::UI::WindowsAndMessaging::GWLP_USERDATA,
        ) as *mut Box<dyn Fn(u64) + Send>;
        if !ptr.is_null() {
            drop(Box::from_raw(ptr));
        }
        let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd);
    }
    Ok(())
}

/// 消息专窗的窗口过程：只处理 `WM_CLIPBOARDUPDATE`。
unsafe extern "system" fn window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_CLIPBOARDUPDATE {
        let seq = u64::from(GetClipboardSequenceNumber());
        let ptr = windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(
            hwnd,
            windows::Win32::UI::WindowsAndMessaging::GWLP_USERDATA,
        ) as *const Box<dyn Fn(u64) + Send>;
        if !ptr.is_null() {
            // SAFETY: 指针由 run_message_loop 在窗口销毁前保持有效；窗口过程与消息循环同线程。
            (*ptr)(seq);
        }
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// 便捷：同步读某个格式的全部数据（内部按块拼接，适合小对象；大对象请用 `read_block` 分块）。
pub fn read_all(backend: &dyn ClipboardBackend, format_id: u32, chunk: usize) -> Result<Vec<u8>> {
    let chunk = chunk.max(1);
    let mut out = Vec::new();
    let mut offset = 0u64;
    loop {
        let block = backend.read_block(format_id, offset, chunk)?;
        if block.block.is_empty() {
            break;
        }
        out.extend_from_slice(&block.block);
        offset = block.offset + block.block.len() as u64;
        if offset >= block.total_len {
            break;
        }
    }
    Ok(out)
}

/// 便于 FFI 层：把 `*mut c_void` 空指针判断包一层（避免调用方直接写 unsafe）。
#[allow(dead_code)]
pub fn is_null_ptr(ptr: *mut c_void) -> bool {
    ptr.is_null()
}

const _: () = {
    // 编译期提醒：这些常量必须与 format.rs 的名字表一致
    let _ = CF_TEXT;
    let _ = CF_DIB;
};
/// ????:???????(? [`ClipboardWatcher::spawn`] ??)?
pub fn watch(callback: impl Fn(u64) + Send + 'static) -> Result<ClipboardWatcher> {
    ClipboardWatcher::spawn(callback)
}
