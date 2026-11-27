//! 把剪贴板拉模式状态机接进 agent / view 会话。
//!
//! 设计见 `design/021-clipboard-sync.md`：谁被改动谁广播 `ClipboardFormatList`，
//! 对端按优先级挑一项发 `ClipboardPull`，源端回 `DataChunk` 分块，对端写回后发 `Applied`。
//!
//! 会话层只提供两条缝：控制流回调（`CtrlHandler::on_ctrl` / `drain_outbox`）与 100ms 心跳 tick
//! （`CtrlHandler::on_tick`）。剪贴板状态机的本地轮询就挂在 tick 上，远端消息在 on_ctrl 里处理，
//! 一次处理可能产生多条回复（分块），因此走 `outbox` 队列而不是单条返回值。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use dfy_clipboard::{message_from_envelope, message_to_envelope, ClipboardSync, SyncMessage};
use dfy_proto::consts::{PERM_CLIPBOARD, PERM_CONTROL, PERM_FILE, PERM_VIEW};
use dfy_proto::Envelope;
use dfy_session::CtrlHandler;
use tracing::{debug, warn};

use crate::file_bridge::{is_file_envelope, AgentFiles, FileStats};
use crate::input_bridge::InjectHandler;

/// 本地剪贴板轮询间隔（远端格式清单变化不会主动推送，只能轮询序列号）。
pub const CLIPBOARD_POLL: Duration = Duration::from_millis(500);

/// 剪贴板桥接统计。
#[derive(Debug, Default, Clone, Copy)]
pub struct ClipboardStats {
    /// 发出的格式清单数。
    pub lists_sent: u64,
    /// 发出的拉取请求数。
    pub pulls_sent: u64,
    /// 发出的数据分块数。
    pub chunks_sent: u64,
    /// 发出的写入确认数。
    pub applied_sent: u64,
    /// 收到的格式清单数。
    pub lists_received: u64,
    /// 收到的拉取请求数。
    pub pulls_received: u64,
    /// 收到的数据分块数。
    pub chunks_received: u64,
    /// 收到的写入确认数。
    pub applied_received: u64,
    /// 发往对端的剪贴板字节数（载荷，不含信封开销）。
    pub bytes_sent: u64,
    /// 从对端收到的剪贴板字节数。
    pub bytes_received: u64,
}

impl ClipboardStats {
    fn record_sent(&mut self, msg: &SyncMessage) {
        match msg {
            SyncMessage::FormatList { .. } => self.lists_sent += 1,
            SyncMessage::Pull { .. } => self.pulls_sent += 1,
            SyncMessage::DataChunk { data, .. } => {
                self.chunks_sent += 1;
                self.bytes_sent += data.len() as u64;
            }
            SyncMessage::Applied { .. } => self.applied_sent += 1,
        }
    }

    fn record_received(&mut self, msg: &SyncMessage) {
        match msg {
            SyncMessage::FormatList { .. } => self.lists_received += 1,
            SyncMessage::Pull { .. } => self.pulls_received += 1,
            SyncMessage::DataChunk { data, .. } => {
                self.chunks_received += 1;
                self.bytes_received += data.len() as u64;
            }
            SyncMessage::Applied { .. } => self.applied_received += 1,
        }
    }
}

/// 创建本地剪贴板同步状态机；平台不支持时返回 `None` 并打印一行原因（不中断会话）。
pub fn open_clipboard() -> Option<ClipboardSync> {
    match dfy_clipboard::create_backend() {
        Ok(backend) => match ClipboardSync::for_session(backend) {
            Ok(sync) => Some(sync),
            Err(err) => {
                println!("[clipboard] 无法建立剪贴板会话基线：{err:#}；本次会话不同步剪贴板");
                None
            }
        },
        Err(err) => {
            println!("[clipboard] 本地剪贴板后端不可用：{err:#}；本次会话不同步剪贴板");
            None
        }
    }
}

/// 把状态机产出的一条或多条消息转成控制帧。
pub fn to_envelopes(msgs: Vec<SyncMessage>) -> Vec<Envelope> {
    msgs.iter().filter_map(message_to_envelope).collect()
}

/// Agent 侧处理器：输入注入 + 剪贴板同步（两者都可单独关闭）。
#[derive(Clone)]
pub struct AgentHandler {
    input: InjectHandler,
    clipboard: Arc<Mutex<Option<ClipboardSync>>>,
    files: Arc<Mutex<Option<AgentFiles>>>,
    outbox: Vec<Envelope>,
    stats: Arc<Mutex<ClipboardStats>>,
    file_stats: Arc<Mutex<FileStats>>,
    last_poll: Instant,
}

impl AgentHandler {
    /// `allow_input` / `allow_clipboard` 为 `false` 时对应功能降级为关闭；
    /// `recv_dir` 为 `Some` 时启用文件接收。
    pub fn new(allow_input: bool, allow_clipboard: bool, recv_dir: Option<PathBuf>) -> Self {
        Self::build(allow_input, allow_clipboard, recv_dir, None)
    }

    /// Creates a handler whose input backend follows the latest isolated X11 display.
    /// Physical clipboard access is deliberately not inherited by a virtual desktop.
    #[cfg(target_os = "linux")]
    pub fn new_on_display_shared(
        allow_input: bool,
        allow_clipboard: bool,
        recv_dir: Option<PathBuf>,
        endpoint: Arc<Mutex<String>>,
    ) -> Self {
        if allow_clipboard {
            warn!("虚拟显示使用独立剪贴板；未继承物理桌面剪贴板同步");
        }
        let input = if allow_input {
            match InjectHandler::on_display_shared(endpoint) {
                Ok(handler) => handler,
                Err(err) => {
                    warn!(error = %err, "输入注入后端不可用，本次会话只串流");
                    InjectHandler::disabled()
                }
            }
        } else {
            InjectHandler::disabled()
        };
        let mut handler = Self::build(false, false, recv_dir, None);
        handler.input = input;
        handler
    }

    fn build(
        allow_input: bool,
        allow_clipboard: bool,
        recv_dir: Option<PathBuf>,
        #[allow(unused_variables)] display: Option<&str>,
    ) -> Self {
        let input = if allow_input {
            let created = {
                #[cfg(target_os = "linux")]
                {
                    display.map_or_else(
                        || InjectHandler::new().map_err(|err| err.context("创建输入注入后端失败")),
                        InjectHandler::on_display,
                    )
                }
                #[cfg(not(target_os = "linux"))]
                {
                    let _ = display;
                    InjectHandler::new()
                }
            };
            match created {
                Ok(handler) => handler,
                Err(err) => {
                    warn!(error = %err, "输入注入后端不可用，本次会话只串流");
                    InjectHandler::disabled()
                }
            }
        } else {
            InjectHandler::disabled()
        };
        let clipboard = if allow_clipboard {
            open_clipboard()
        } else {
            None
        };
        let files = match recv_dir {
            Some(dir) => match AgentFiles::new(&dir, dfy_files::OverwritePolicy::Rename) {
                Ok(files) => Some(files),
                Err(err) => {
                    warn!(error = %err, "文件接收后端初始化失败，本次会话不接收文件");
                    None
                }
            },
            None => None,
        };
        Self {
            input,
            clipboard: Arc::new(Mutex::new(clipboard)),
            files: Arc::new(Mutex::new(files)),
            outbox: Vec::new(),
            stats: Arc::new(Mutex::new(ClipboardStats::default())),
            file_stats: Arc::new(Mutex::new(FileStats::default())),
            last_poll: Instant::now(),
        }
    }

    /// 输入注入是否可用。
    pub fn is_enabled(&self) -> bool {
        self.input.is_enabled()
    }

    /// 剪贴板同步是否可用。
    pub fn clipboard_enabled(&self) -> bool {
        self.clipboard.lock().map(|c| c.is_some()).unwrap_or(false)
    }

    /// 本连接实际启用的权限位。
    pub fn permission_bitmap(&self) -> u32 {
        let mut permissions = PERM_VIEW;
        if self.is_enabled() {
            permissions |= PERM_CONTROL;
        }
        if self.clipboard_enabled() {
            permissions |= PERM_CLIPBOARD;
        }
        if self.files_enabled() {
            permissions |= PERM_FILE;
        }
        permissions
    }

    /// 输入注入统计快照。
    pub fn stats(&self) -> crate::input_bridge::InjectStats {
        self.input.stats()
    }

    /// 文件接收是否可用。
    pub fn files_enabled(&self) -> bool {
        self.files.lock().map(|f| f.is_some()).unwrap_or(false)
    }

    /// 文件接收统计快照。
    pub fn file_stats(&self) -> FileStats {
        self.file_stats.lock().map(|s| *s).unwrap_or_default()
    }

    /// 剪贴板统计快照。
    pub fn clipboard_stats(&self) -> ClipboardStats {
        self.stats.lock().map(|s| *s).unwrap_or_default()
    }

    /// 会话结束：释放可能仍按下的键。
    pub fn release(&self) {
        self.input.release();
    }

    fn push(&mut self, msg: SyncMessage) {
        let Some(env) = message_to_envelope(&msg) else {
            debug!("剪贴板消息无法映射为信封，丢弃");
            return;
        };
        if let Ok(mut stats) = self.stats.lock() {
            stats.record_sent(&msg);
        }
        self.outbox.push(env);
    }

    fn poll_clipboard(&mut self) {
        let polled = {
            let Ok(mut guard) = self.clipboard.lock() else {
                return;
            };
            let Some(sync) = guard.as_mut() else { return };
            sync.poll_local()
        };
        match polled {
            Ok(Some(msg)) => {
                debug!("本地剪贴板变化，广播格式清单");
                self.push(msg);
            }
            Ok(None) => {}
            Err(err) => debug!(error = %err, "读取本地剪贴板失败"),
        }
    }
}

impl CtrlHandler for AgentHandler {
    fn on_ctrl(&mut self, env: &Envelope) -> Result<Option<Envelope>> {
        if is_file_envelope(env) {
            let replies = {
                let Ok(mut guard) = self.files.lock() else {
                    return Ok(None);
                };
                match guard.as_mut() {
                    Some(files) => files.on_envelope(env),
                    None => Vec::new(),
                }
            };
            if let Ok(mut stats) = self.file_stats.lock() {
                if let Ok(guard) = self.files.lock() {
                    if let Some(files) = guard.as_ref() {
                        *stats = files.stats();
                    }
                }
            }
            self.outbox.extend(replies);
            return Ok(None);
        }
        if let Some(msg) = message_from_envelope(env) {
            if let Ok(mut stats) = self.stats.lock() {
                stats.record_received(&msg);
            }
            let replies = {
                let Ok(mut guard) = self.clipboard.lock() else {
                    return Ok(None);
                };
                match guard.as_mut() {
                    Some(sync) => match sync.on_remote(&msg) {
                        Ok(replies) => replies,
                        Err(err) => {
                            warn!(error = %err, "处理远端剪贴板消息失败");
                            Vec::new()
                        }
                    },
                    None => Vec::new(),
                }
            };
            for reply in replies {
                self.push(reply);
            }
            return Ok(None);
        }
        self.input.on_ctrl(env)
    }

    fn on_datagram(&mut self, chan: u8, payload: &[u8]) -> Result<()> {
        self.input.on_datagram(chan, payload)
    }

    fn on_tick(&mut self) -> Result<()> {
        if self.last_poll.elapsed() >= CLIPBOARD_POLL {
            self.last_poll = Instant::now();
            self.poll_clipboard();
        }
        Ok(())
    }

    fn drain_outbox(&mut self) -> Vec<Envelope> {
        std::mem::take(&mut self.outbox)
    }
}

/// View 侧剪贴板同步：本地轮询 + 处理远端消息，返回需要发回 Agent 的帧。
pub struct ViewClipboard {
    sync: ClipboardSync,
    last_poll: Instant,
    stats: ClipboardStats,
}

impl ViewClipboard {
    /// 创建状态机；平台不支持时返回 `None`。
    pub fn new() -> Option<Self> {
        open_clipboard().map(|sync| Self {
            sync,
            last_poll: Instant::now(),
            stats: ClipboardStats::default(),
        })
    }

    /// 处理一条包含剪贴板消息的控制帧；返回需要发回的帧。
    pub fn on_ctrl(&mut self, env: &Envelope) -> Vec<Envelope> {
        let Some(msg) = message_from_envelope(env) else {
            return Vec::new();
        };
        self.stats.record_received(&msg);
        match self.sync.on_remote(&msg) {
            Ok(replies) => {
                for reply in &replies {
                    self.stats.record_sent(reply);
                }
                to_envelopes(replies)
            }
            Err(err) => {
                warn!(error = %err, "处理远端剪贴板消息失败");
                Vec::new()
            }
        }
    }

    /// 到轮询点时读取本地剪贴板；返回需要发回的帧。
    pub fn poll(&mut self) -> Vec<Envelope> {
        if self.last_poll.elapsed() < CLIPBOARD_POLL {
            return Vec::new();
        }
        self.last_poll = Instant::now();
        match self.sync.poll_local() {
            Ok(Some(msg)) => {
                self.stats.record_sent(&msg);
                to_envelopes(vec![msg])
            }
            Ok(None) => Vec::new(),
            Err(err) => {
                debug!(error = %err, "读取本地剪贴板失败");
                Vec::new()
            }
        }
    }

    /// 统计快照。
    pub fn stats(&self) -> ClipboardStats {
        self.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_count_by_variant() {
        let mut s = ClipboardStats::default();
        s.record_received(&SyncMessage::Pull { hash: [1u8; 32] });
        s.record_received(&SyncMessage::DataChunk {
            hash: [1u8; 32],
            offset: 0,
            data: vec![0u8; 10],
            last: true,
        });
        assert_eq!(s.pulls_received, 1);
        assert_eq!(s.chunks_received, 1);
        assert_eq!(s.bytes_received, 10);
    }

    #[test]
    fn disabled_handler_has_empty_outbox() {
        let mut handler = AgentHandler::new(false, false, None);
        assert!(handler.drain_outbox().is_empty());
        assert!(!handler.is_enabled());
        assert!(!handler.clipboard_enabled());
        assert!(!handler.files_enabled());
        assert_eq!(handler.permission_bitmap(), PERM_VIEW);
    }
}
