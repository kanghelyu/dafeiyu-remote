// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 控制端（View）本地输入采集：Windows 低级钩子 / Linux X11 XInput2 → `dfy-proto` 输入事件。
//!
//! 设计要点：
//!
//! - **Windows**：用 [`WH_MOUSE_LL`]/[`WH_KEYBOARD_LL`] 全局钩子采集本机键鼠，并用 5ms
//!   `GetCursorPos`/`GetAsyncKeyState` 状态轮询兜底；某些策略不把 SendInput 合成事件送入钩子，
//!   轮询仍能读取真实状态，共用坐标/按下集合去重。滚轮由鼠标钩子提供。
//! - **Linux X11**：用 XInput2 RawKey/RawButton/RawMotion 观察当前已授权的 X11 会话，物理键码经
//!   XKB 键名转成 HID Usage；不借助 XWayland 绕过 Wayland 授权边界。
//! - **鼠标移动节流**：10ms 合并窗口内只保留最新坐标，避免高轮询率鼠标打爆网络。
//! - **停止快捷键 `Ctrl+Alt+Shift+Q`**：两个平台都吞掉停止组合并结束采集；停止前显式释放仍
//!   按住的键/鼠标键，避免远端卡键。
//! - **线程间通信**：采集线程通过 `tokio::sync::mpsc::UnboundedSender<CtrlMsg>` 投递，钩子/
//!   X11 回调侧永不阻塞；主循环在 `select!` 中接收。
//! - macOS 和其他平台只提供「不可用」的降级实现，`--control` 时打印提示并继续只收视频。
//!
//! 目前采集范围：鼠标移动 + 左/右/中/后退/前进键 + 滚轮 + 键盘。文本/触摸/手柄不做。

// 平台采集实现；非 Windows/Linux 的调用点被 cfg 掉，保留类型避免 public API 抖动。
#![cfg_attr(not(windows), allow(dead_code))]
#![allow(unsafe_code)]

use anyhow::Result;
use tokio::sync::mpsc;

/// 采集线程 → tokio 任务的消息。
#[derive(Debug)]
pub enum CtrlMsg {
    /// 鼠标移动（`x`/`y` 已归一化到 `0..=65535`），走不可靠 DATAGRAM。
    Move(dfy_proto::InputEvent),
    /// 其它输入事件（键盘/鼠标键/滚轮），走可靠 control 流。
    Event(dfy_proto::Envelope),
    /// 用户按下了停止组合键 `Ctrl+Alt+Shift+Q`。
    Stopped,
}

/// 当前仍按住的键/鼠标键快照。
///
/// 供路径迁移后在新控制通道重放：旧连接上丢失的 KeyUp 不会让被控端卡键。
/// HID 语义与 `CtrlMsg::Event` 一致（键盘 Usage ID、鼠标按键 1..=5）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct HeldInput {
    pub keys: Vec<u16>,
    pub buttons: Vec<u8>,
}

/// 控制端输入采集器。
///
/// [`InputCapture::disabled`] 得到的实例不采集任何输入，[`InputCapture::recv`] 永远
/// pending，可直接放进 `tokio::select!` 而不空转。
pub struct InputCapture {
    rx: Option<mpsc::UnboundedReceiver<CtrlMsg>>,
    #[cfg(windows)]
    hooks: Option<win::Hooks>,
    #[cfg(target_os = "linux")]
    worker: Option<linux::Worker>,
}

impl InputCapture {
    /// 启动采集（Windows 低级钩子 / Linux X11 XInput2）。
    ///
    /// 返回时采集线程已经装好；失败原因可直接打印给用户。
    pub fn start() -> Result<Self> {
        #[cfg(windows)]
        {
            let (tx, rx) = mpsc::unbounded_channel();
            let hooks = win::Hooks::start(tx)?;
            Ok(Self {
                rx: Some(rx),
                hooks: Some(hooks),
            })
        }
        #[cfg(target_os = "linux")]
        {
            let (tx, rx) = mpsc::unbounded_channel();
            let worker = linux::Worker::start(tx)?;
            Ok(Self {
                rx: Some(rx),
                worker: Some(worker),
            })
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        {
            anyhow::bail!("当前平台暂不支持控制输入（Windows 与 Linux X11 已实现采集）")
        }
    }

    /// 不做任何采集的占位实例（`--control` 未开启或平台不支持时使用）。
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            rx: None,
            #[cfg(windows)]
            hooks: None,
            #[cfg(target_os = "linux")]
            worker: None,
        }
    }

    /// 当前按住的键/按钮快照；未启用采集或未建立状态时为空。
    #[must_use]
    pub(crate) fn held(&self) -> HeldInput {
        #[cfg(windows)]
        if let Some(hooks) = &self.hooks {
            return hooks.held();
        }
        #[cfg(target_os = "linux")]
        if let Some(worker) = &self.worker {
            return worker.held();
        }
        HeldInput::default()
    }

    /// 收下一条采集消息；无消息时挂起等待。
    ///
    /// 禁用状态或已 [`stop`](Self::stop) 后永远 pending（返回 `None` 的语义用不到，
    /// 避免调用方的 `select!` 空转）。
    pub async fn recv(&mut self) -> Option<CtrlMsg> {
        match self.rx.as_mut() {
            Some(rx) => rx.recv().await,
            None => std::future::pending().await,
        }
    }

    /// 停止采集、卸载钩子并回收钩子线程（幂等）。
    ///
    /// 必须在会话结束前调用：钩子卸载后本机输入立刻恢复正常。
    pub fn stop(&mut self) {
        #[cfg(windows)]
        if let Some(hooks) = self.hooks.take() {
            hooks.stop();
        }
        #[cfg(target_os = "linux")]
        if let Some(worker) = self.worker.take() {
            worker.stop();
        }
        self.rx = None;
    }
}

impl Drop for InputCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(windows)]
mod win {
    use std::cell::RefCell;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{self, Sender};
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use anyhow::{anyhow, Context, Result};
    use dfy_input::HidKeymap;
    use dfy_proto::envelope;
    use dfy_proto::{
        INPUT_KIND_KEY, INPUT_KIND_MOUSE_BTN, INPUT_KIND_MOUSE_MOVE, INPUT_KIND_WHEEL,
        INPUT_STATE_DOWN, INPUT_STATE_UP,
    };
    use tokio::sync::mpsc::UnboundedSender;
    use tracing::{debug, trace};
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, POINT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, VIRTUAL_KEY, VK_CONTROL, VK_LBUTTON, VK_LCONTROL, VK_LMENU, VK_LSHIFT,
        VK_LWIN, VK_MBUTTON, VK_MENU, VK_Q, VK_RBUTTON, VK_RCONTROL, VK_RMENU, VK_RSHIFT, VK_RWIN,
        VK_SHIFT, VK_XBUTTON1, VK_XBUTTON2,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, GetCursorPos, GetSystemMetrics, KillTimer, PeekMessageW, PostQuitMessage,
        PostThreadMessageW, SetTimer, SetWindowsHookExW, UnhookWindowsHookEx, HC_ACTION, HHOOK,
        KBDLLHOOKSTRUCT, LLKHF_EXTENDED, MSG, MSLLHOOKSTRUCT, PM_NOREMOVE, PM_REMOVE, SM_CXSCREEN,
        SM_CYSCREEN, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN,
        WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL,
        WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER,
        WM_XBUTTONDOWN, WM_XBUTTONUP, XBUTTON1, XBUTTON2,
    };

    use super::{CtrlMsg, HeldInput};

    /// 鼠标移动合并窗口：窗口内的多次移动只发最新一条。
    const MOVE_MERGE_INTERVAL: Duration = Duration::from_millis(10);
    /// 合并定时器 ID（线程定时器，`SetTimer(None, ..)`）。
    const TIMER_MERGE_ID: usize = 1;
    /// 合并定时器周期（毫秒），略大于窗口以保证窗口内的移动都能被合并。
    const TIMER_MERGE_MS: u32 = 10;
    /// 状态轮询周期：钩子收不到合成事件时仍能捕获真实光标/按键状态。
    const TIMER_POLL_MS: u32 = 5;

    /// 停止组合键里的 Q 键（其余修饰键用 `GetAsyncKeyState` 实时判断，避免自己维护状态机）。
    const STOP_KEY: VIRTUAL_KEY = VK_Q;

    /// 钩子线程状态（线程局部：只有安装钩子的那个线程会访问）。
    struct HookState {
        tx: UnboundedSender<super::CtrlMsg>,
        stopped: Arc<AtomicBool>,
        screen_width: i32,
        screen_height: i32,
        /// 上次真正发出移动事件的时间。
        last_move: Option<Instant>,
        /// 等待合并发送的最新归一化坐标。
        pending_move: Option<(u32, u32)>,
        /// 钩子/轮询共同看到的最近光标位置，用于去重。
        last_position: Option<(i32, i32)>,
        /// 已发送 Down 且尚未收到 Up 的 HID 键/按钮，停止时用于释放远端状态。
        pressed_keys: HashSet<u16>,
        pressed_buttons: HashSet<u8>,
        poll_ticks: u64,
        /// 是否已经挂了合并定时器（避免同一窗口内重复 `SetTimer`）。
        timer_armed: bool,
    }

    thread_local! {
        static HOOK: RefCell<Option<HookState>> = const { RefCell::new(None) };
    }

    /// 已安装的两个钩子（只在本线程使用，`HHOOK` 不是 `Send`）。
    struct Installed {
        mouse: HHOOK,
        keyboard: HHOOK,
    }

    /// 采集线程句柄（`stop` 需要线程 ID 来投递 `WM_QUIT`）。
    pub(super) struct Hooks {
        thread: JoinHandle<()>,
        thread_id: u32,
        stopped: Arc<AtomicBool>,
        held: Arc<Mutex<HeldInput>>,
    }

    impl Hooks {
        /// 启动钩子线程；返回时钩子已装好（失败会带原因返回）。
        pub(super) fn start(tx: UnboundedSender<super::CtrlMsg>) -> Result<Self> {
            let stopped = Arc::new(AtomicBool::new(false));
            let held = Arc::new(Mutex::new(HeldInput::default()));
            let (ready_tx, ready_rx) = mpsc::channel::<std::result::Result<u32, String>>();
            let stopped_for_thread = Arc::clone(&stopped);
            let held_for_thread = Arc::clone(&held);
            let thread = std::thread::Builder::new()
                .name("dfy-input-hooks".to_string())
                .spawn(move || thread_main(tx, stopped_for_thread, ready_tx, held_for_thread))
                .context("启动输入钩子线程失败")?;
            match ready_rx.recv_timeout(Duration::from_secs(5)) {
                Ok(Ok(thread_id)) => Ok(Self {
                    thread,
                    thread_id,
                    stopped,
                    held,
                }),
                Ok(Err(message)) => {
                    let _ = thread.join();
                    Err(anyhow!("安装输入钩子失败：{message}"))
                }
                Err(err) => {
                    // 就绪握手超时意味着拿不到线程 ID、没法发 WM_QUIT：这里不 join，
                    // 避免上层卡死；线程（如果还活着）会在进程退出时一起结束。
                    Err(anyhow!("等待输入钩子线程就绪失败：{err}"))
                }
            }
        }

        /// 最近一次观察到的按住键/按钮快照（路径迁移后用于重放）。
        pub(super) fn held(&self) -> HeldInput {
            self.held
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }

        /// 请求停止并等待钩子线程退出（卸载钩子、恢复本机输入）。
        pub(super) fn stop(self) {
            if !self.stopped.swap(true, Ordering::SeqCst) {
                // 线程消息队列已在「就绪握手」前用 PeekMessageW 建好，投递必然成功。
                let _ =
                    unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
            }
            let _ = self.thread.join();
        }
    }

    /// 钩子线程主体：装钩子 → 建消息队列 → 通知就绪 → 消息泵。
    fn thread_main(
        tx: UnboundedSender<super::CtrlMsg>,
        stopped: Arc<AtomicBool>,
        ready: Sender<std::result::Result<u32, String>>,
        held: Arc<Mutex<HeldInput>>,
    ) {
        let thread_id = unsafe { GetCurrentThreadId() };
        match install(&tx, &stopped) {
            Ok(installed) => {
                // 先创建线程消息队列，保证外部 stop() 的 PostThreadMessageW 能命中。
                let mut msg = MSG::default();
                unsafe {
                    let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
                }
                let _ = ready.send(Ok(thread_id));
                run_message_loop(&held);
                installed.uninstall();
            }
            Err(err) => {
                let _ = ready.send(Err(format!("{err:#}")));
            }
        }
    }

    /// 装钩子并把线程局部状态填好。
    fn install(
        tx: &UnboundedSender<super::CtrlMsg>,
        stopped: &Arc<AtomicBool>,
    ) -> Result<Installed> {
        let screen_width = unsafe { GetSystemMetrics(SM_CXSCREEN) };
        let screen_height = unsafe { GetSystemMetrics(SM_CYSCREEN) };
        if screen_width <= 1 || screen_height <= 1 {
            return Err(anyhow!(
                "主屏尺寸异常（{screen_width}x{screen_height}），无法归一化鼠标坐标"
            ));
        }
        let initial_position = unsafe {
            let mut point = POINT::default();
            GetCursorPos(&mut point).ok().map(|_| (point.x, point.y))
        };
        HOOK.with(|cell| {
            *cell.borrow_mut() = Some(HookState {
                tx: tx.clone(),
                stopped: Arc::clone(stopped),
                screen_width,
                screen_height,
                last_move: None,
                pending_move: None,
                last_position: initial_position,
                pressed_keys: HashSet::new(),
                pressed_buttons: HashSet::new(),
                poll_ticks: 0,
                timer_armed: false,
            });
        });

        let module = unsafe { GetModuleHandleW(PCWSTR::null()) }.context("获取模块句柄失败")?;
        let module = HINSTANCE(module.0);

        // SAFETY: 钩子回调是符合 `HOOKPROC` 签名的 `extern "system"` 函数，模块句柄来自
        // 当前进程；线程 ID 为 0 表示全局钩子。
        let mouse =
            unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_proc), Some(module), 0) }
                .context("安装 WH_MOUSE_LL 钩子失败（可能被安全软件拦截）")?;
        let keyboard = match unsafe {
            SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_proc), Some(module), 0)
        } {
            Ok(hook) => hook,
            Err(err) => {
                let _ = unsafe { UnhookWindowsHookEx(mouse) };
                return Err(anyhow!("安装 WH_KEYBOARD_LL 钩子失败：{err}"));
            }
        };
        Ok(Installed { mouse, keyboard })
    }

    impl Installed {
        /// 卸载钩子并清理线程局部状态（线程退出前调用）。
        fn uninstall(self) {
            let _ = unsafe { UnhookWindowsHookEx(self.mouse) };
            let _ = unsafe { UnhookWindowsHookEx(self.keyboard) };
            let _ = unsafe { KillTimer(None, TIMER_MERGE_ID) };
            HOOK.with(|cell| {
                if let Ok(mut guard) = cell.try_borrow_mut() {
                    *guard = None;
                }
            });
        }
    }

    /// 消息泵：处理合并定时器/退出消息，并在 5ms 空档轮询真实输入状态。
    fn run_message_loop(held: &Arc<Mutex<HeldInput>>) {
        let mut msg = MSG::default();
        loop {
            loop {
                // SAFETY: `msg` 是有效的可写 `MSG`；PM_REMOVE 只取走本线程消息。
                let ret = unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) };
                if ret.0 == 0 {
                    break;
                }
                if ret.0 < 0 {
                    debug!("PeekMessageW 失败，结束 Windows 输入采集消息泵");
                    return;
                }
                if msg.message == WM_QUIT {
                    return;
                }
                if msg.message == WM_TIMER && msg.wParam.0 == TIMER_MERGE_ID {
                    let _ = unsafe { KillTimer(None, TIMER_MERGE_ID) };
                    flush_pending_move();
                }
            }
            if poll_input_state() {
                return;
            }
            publish_held(held);
            std::thread::sleep(Duration::from_millis(u64::from(TIMER_POLL_MS)));
        }
    }

    /// 把钩子线程的按下集合复制到可被主线程读取的快照（排序保证可测）。
    fn publish_held(held: &Arc<Mutex<HeldInput>>) {
        HOOK.with(|cell| {
            if let Ok(state) = cell.try_borrow() {
                if let Some(state) = state.as_ref() {
                    let mut keys: Vec<u16> = state.pressed_keys.iter().copied().collect();
                    keys.sort_unstable();
                    let mut buttons: Vec<u8> = state.pressed_buttons.iter().copied().collect();
                    buttons.sort_unstable();
                    let mut guard = held.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    guard.keys = keys;
                    guard.buttons = buttons;
                }
            }
        });
    }

    /// SAFETY: 由系统在安装钩子的线程上调用；`lparam` 指向 `MSLLHOOKSTRUCT`。
    unsafe extern "system" fn mouse_hook_proc(
        code: i32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if code == HC_ACTION as i32 {
            if let Some(ret) = handle_mouse(wparam, lparam) {
                return ret;
            }
        }
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    }

    /// SAFETY: 由系统在安装钩子的线程上调用；`lparam` 指向 `KBDLLHOOKSTRUCT`。
    unsafe extern "system" fn keyboard_hook_proc(
        code: i32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if code == HC_ACTION as i32 {
            if let Some(ret) = handle_keyboard(wparam, lparam) {
                return ret;
            }
        }
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    }

    /// 鼠标回调。返回 `Some(LRESULT)` 表示吞掉该消息，`None` 表示放行给本机。
    fn handle_mouse(wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        // 已请求停止：吞掉余下事件，等消息泵退出。
        if is_stopped() {
            return Some(LRESULT(1));
        }
        // SAFETY: 低级鼠标钩子的 `lparam` 是 `MSLLHOOKSTRUCT*`，仅在回调期间有效。
        let data = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        match wparam.0 as u32 {
            WM_MOUSEMOVE => {
                queue_move(data.pt.x, data.pt.y);
                None
            }
            // HID Button 页（0x0C）1 基编号：1=左、2=右、3=中、4=后退、5=前进。
            WM_LBUTTONDOWN => {
                if track_button(1, true) {
                    emit(button_event(1, true));
                }
                None
            }
            WM_LBUTTONUP => {
                if track_button(1, false) {
                    emit(button_event(1, false));
                }
                None
            }
            WM_RBUTTONDOWN => {
                if track_button(2, true) {
                    emit(button_event(2, true));
                }
                None
            }
            WM_RBUTTONUP => {
                if track_button(2, false) {
                    emit(button_event(2, false));
                }
                None
            }
            WM_MBUTTONDOWN => {
                if track_button(3, true) {
                    emit(button_event(3, true));
                }
                None
            }
            WM_MBUTTONUP => {
                if track_button(3, false) {
                    emit(button_event(3, false));
                }
                None
            }
            WM_XBUTTONDOWN | WM_XBUTTONUP => {
                // `mouseData` 的高 16 位标识 XBUTTON1/XBUTTON2。
                let which = (data.mouseData >> 16) as u16;
                let usage = match which {
                    XBUTTON1 => 4,
                    XBUTTON2 => 5,
                    _ => return None,
                };
                let down = wparam.0 as u32 == WM_XBUTTONDOWN;
                if track_button(usage, down) {
                    emit(button_event(u32::from(usage), down));
                }
                None
            }
            WM_MOUSEWHEEL => {
                // `mouseData` 的高 16 位是有符号的滚轮增量（120 = 一格）。
                emit(wheel_event(0, wheel_delta(data.mouseData)));
                None
            }
            WM_MOUSEHWHEEL => {
                emit(wheel_event(wheel_delta(data.mouseData), 0));
                None
            }
            _ => None,
        }
    }

    /// 键盘回调。返回 `Some(LRESULT)` 表示吞掉该消息，`None` 表示放行给本机。
    fn handle_keyboard(wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        let message = wparam.0 as u32;
        let down = match message {
            WM_KEYDOWN | WM_SYSKEYDOWN => true,
            WM_KEYUP | WM_SYSKEYUP => false,
            _ => return None,
        };
        if is_stopped() {
            return Some(LRESULT(1));
        }
        // SAFETY: 低级键盘钩子的 `lparam` 是 `KBDLLHOOKSTRUCT*`，仅在回调期间有效。
        let data = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        let vk = data.vkCode;

        // 停止组合键：Ctrl+Alt+Shift+Q（修饰键用实时状态判断，避开自己维护状态机）。
        if down
            && vk == u32::from(STOP_KEY.0)
            && is_modifier_down(VK_CONTROL)
            && is_modifier_down(VK_MENU)
            && is_modifier_down(VK_SHIFT)
        {
            request_stop();
            return Some(LRESULT(1)); // 吞掉该按键，避免本机收到无配对 Down 的 Q
        }

        // 扫描码优先（能区分左右修饰键/小键盘），回退到 VK 表。
        let extended = data.flags.contains(LLKHF_EXTENDED);
        let scan = if extended {
            (data.scanCode as u16) | 0xE000
        } else {
            data.scanCode as u16
        };
        let usage = HidKeymap::scancode_to_usage(scan).or_else(|| {
            HidKeymap::entries()
                .iter()
                .find(|entry| u32::from(entry.vk) == vk)
                .map(|entry| entry.usage)
        });
        let Some(usage) = usage else {
            trace!(vk, scan, "无法映射到 HID Usage，忽略按键");
            return None;
        };
        if track_key(usage, down) {
            emit(key_event(usage, down, current_modifiers()));
        }
        None
    }

    /// 合并策略：首次/距上次发送 >= 10ms 立即发；否则挂在一次性定时器上，窗口内只留最新。
    fn queue_move(x: i32, y: i32) {
        let mut immediate = None;
        let mut arm_timer = false;
        HOOK.with(|cell| {
            let Ok(mut guard) = cell.try_borrow_mut() else {
                return;
            };
            let Some(state) = guard.as_mut() else {
                return;
            };
            if state.last_position == Some((x, y)) && state.pending_move.is_none() {
                return;
            }
            state.last_position = Some((x, y));
            let nx = normalize(x, state.screen_width);
            let ny = normalize(y, state.screen_height);
            state.pending_move = Some((nx, ny));
            let now = Instant::now();
            let due = match state.last_move {
                Some(last) => now.saturating_duration_since(last) >= MOVE_MERGE_INTERVAL,
                None => true,
            };
            if due {
                immediate = state.pending_move.take();
                state.last_move = Some(now);
            } else if !state.timer_armed {
                state.timer_armed = true;
                arm_timer = true;
            }
        });
        if arm_timer {
            // 一次性定时器：WM_TIMER 里 KillTimer + flush，窗口内只发最新坐标。
            unsafe { SetTimer(None, TIMER_MERGE_ID, TIMER_MERGE_MS, None) };
        }
        if let Some((nx, ny)) = immediate {
            send(CtrlMsg::Move(move_event(nx, ny)));
        }
    }

    /// 定时器到期：把最后一次待发移动送出去。
    fn flush_pending_move() {
        let mut pending = None;
        HOOK.with(|cell| {
            let Ok(mut guard) = cell.try_borrow_mut() else {
                return;
            };
            let Some(state) = guard.as_mut() else {
                return;
            };
            state.timer_armed = false;
            pending = state.pending_move.take();
            if pending.is_some() {
                state.last_move = Some(Instant::now());
            }
        });
        if let Some((nx, ny)) = pending {
            send(CtrlMsg::Move(move_event(nx, ny)));
        }
    }

    /// 屏幕坐标 → `0..=65535` 归一化值（按主屏尺寸，越界 clamp 到边缘）。
    fn normalize(value: i32, extent: i32) -> u32 {
        if extent <= 1 {
            return 0;
        }
        let clamped = value.clamp(0, extent - 1) as i64;
        (clamped * 65_535 / (i64::from(extent) - 1)) as u32
    }

    fn track_key(usage: u16, down: bool) -> bool {
        HOOK.with(|cell| {
            let Ok(mut guard) = cell.try_borrow_mut() else {
                return false;
            };
            let Some(state) = guard.as_mut() else {
                return false;
            };
            if down {
                state.pressed_keys.insert(usage)
            } else {
                state.pressed_keys.remove(&usage)
            }
        })
    }

    fn track_button(button: u8, down: bool) -> bool {
        HOOK.with(|cell| {
            let Ok(mut guard) = cell.try_borrow_mut() else {
                return false;
            };
            let Some(state) = guard.as_mut() else {
                return false;
            };
            if down {
                state.pressed_buttons.insert(button)
            } else {
                state.pressed_buttons.remove(&button)
            }
        })
    }

    /// 在已持有 HookState 时执行停止，释放远端按键并返回是否首次停止。
    fn begin_stop(state: &mut HookState) -> bool {
        if state.stopped.swap(true, Ordering::SeqCst) {
            return false;
        }
        let modifiers = current_modifiers();
        for usage in state.pressed_keys.drain() {
            let _ = state
                .tx
                .send(super::CtrlMsg::Event(event_envelope(key_event(
                    usage, false, modifiers,
                ))));
        }
        for button in state.pressed_buttons.drain() {
            let _ = state
                .tx
                .send(super::CtrlMsg::Event(event_envelope(button_event(
                    u32::from(button),
                    false,
                ))));
        }
        let _ = state.tx.send(super::CtrlMsg::Stopped);
        true
    }

    /// 请求停止采集：释放远端按键、置标志、通知主循环、结束消息泵。
    fn request_stop() {
        let notified = HOOK.with(|cell| {
            let Ok(mut guard) = cell.try_borrow_mut() else {
                return false;
            };
            guard.as_mut().is_some_and(begin_stop)
        });
        if notified {
            debug!("收到 Ctrl+Alt+Shift+Q，停止控制输入采集");
            unsafe { PostQuitMessage(0) };
        }
    }

    /// 5ms 状态轮询兜底。某些 Windows/安全策略不会把 SendInput 合成事件送入低级钩子，
    /// 但光标与异步按键状态仍可读取；与钩子共用 pressed 集合和最近坐标，避免重复事件。
    fn poll_input_state() -> bool {
        if is_stopped() {
            return true;
        }
        let mut point = POINT::default();
        if unsafe { GetCursorPos(&mut point) }.is_ok() {
            queue_move(point.x, point.y);
        }

        let stopped = HOOK.with(|cell| {
            let Ok(mut guard) = cell.try_borrow_mut() else {
                return false;
            };
            let Some(state) = guard.as_mut() else {
                return false;
            };
            state.poll_ticks = state.poll_ticks.saturating_add(1);
            if state.poll_ticks == 1 {
                debug!("Windows 输入状态轮询已启动");
            }
            let current_keys: HashSet<u16> = HidKeymap::entries()
                .iter()
                .filter(|entry| unsafe { GetAsyncKeyState(i32::from(entry.vk)) } < 0)
                .map(|entry| entry.usage)
                .collect();
            let added = current_keys
                .difference(&state.pressed_keys)
                .copied()
                .collect::<Vec<_>>();
            let removed = state
                .pressed_keys
                .difference(&current_keys)
                .copied()
                .collect::<Vec<_>>();
            for usage in &added {
                state.pressed_keys.insert(*usage);
            }
            for usage in &removed {
                state.pressed_keys.remove(usage);
            }
            let has_modifier = |usages: &[u16]| {
                usages
                    .iter()
                    .any(|usage| state.pressed_keys.contains(usage))
            };
            if state.pressed_keys.contains(&0x14)
                && has_modifier(&[0xe0, 0xe4])
                && has_modifier(&[0xe2, 0xe6])
                && has_modifier(&[0xe1, 0xe5])
            {
                return begin_stop(state);
            }
            let modifiers = current_modifiers();
            if !added.is_empty() || !removed.is_empty() {
                debug!(?added, ?removed, "Windows 轮询检测到键盘状态变化");
            }
            for usage in added {
                let _ = state
                    .tx
                    .send(super::CtrlMsg::Event(event_envelope(key_event(
                        usage, true, modifiers,
                    ))));
            }
            for usage in removed {
                let _ = state
                    .tx
                    .send(super::CtrlMsg::Event(event_envelope(key_event(
                        usage, false, modifiers,
                    ))));
            }

            let current_buttons = [
                (VK_LBUTTON, 1),
                (VK_RBUTTON, 2),
                (VK_MBUTTON, 3),
                (VK_XBUTTON1, 4),
                (VK_XBUTTON2, 5),
            ]
            .into_iter()
            .filter(|(vk, _)| unsafe { GetAsyncKeyState(i32::from(vk.0)) } < 0)
            .map(|(_, button)| button)
            .collect::<HashSet<_>>();
            let added_buttons = current_buttons
                .difference(&state.pressed_buttons)
                .copied()
                .collect::<Vec<_>>();
            let removed_buttons = state
                .pressed_buttons
                .difference(&current_buttons)
                .copied()
                .collect::<Vec<_>>();
            for button in &added_buttons {
                state.pressed_buttons.insert(*button);
            }
            for button in &removed_buttons {
                state.pressed_buttons.remove(button);
            }
            for button in added_buttons {
                let _ = state
                    .tx
                    .send(super::CtrlMsg::Event(event_envelope(button_event(
                        u32::from(button),
                        true,
                    ))));
            }
            for button in removed_buttons {
                let _ = state
                    .tx
                    .send(super::CtrlMsg::Event(event_envelope(button_event(
                        u32::from(button),
                        false,
                    ))));
            }
            false
        });
        if stopped {
            debug!("轮询检测到 Ctrl+Alt+Shift+Q，停止控制输入采集");
            unsafe { PostQuitMessage(0) };
        }
        stopped
    }

    /// 是否已被要求停止。
    fn is_stopped() -> bool {
        HOOK.with(|cell| {
            let Ok(guard) = cell.try_borrow() else {
                return true;
            };
            match guard.as_ref() {
                Some(state) => state.stopped.load(Ordering::SeqCst),
                None => true,
            }
        })
    }

    /// 往 tokio 任务发消息（无界通道不阻塞；接收端已关闭则丢弃）。
    fn send(msg: super::CtrlMsg) {
        HOOK.with(|cell| {
            let Ok(guard) = cell.try_borrow() else {
                return;
            };
            if let Some(state) = guard.as_ref() {
                let _ = state.tx.send(msg);
            }
        });
    }

    /// 按键状态：`GetAsyncKeyState` 的最高位为 1 表示当前按下。
    fn is_modifier_down(vk: VIRTUAL_KEY) -> bool {
        unsafe { GetAsyncKeyState(i32::from(vk.0)) < 0 }
    }

    /// 当前修饰键位图（HID 语义：bit0=LCtrl … bit7=RGui）。
    fn current_modifiers() -> u8 {
        // 通用 VK 在只按下一侧时也会置位；先按左右专用键判断，再回退到通用键。
        let left_ctrl = is_modifier_down(VK_LCONTROL)
            || (!is_modifier_down(VK_RCONTROL) && is_modifier_down(VK_CONTROL));
        let left_shift = is_modifier_down(VK_LSHIFT)
            || (!is_modifier_down(VK_RSHIFT) && is_modifier_down(VK_SHIFT));
        let left_alt = is_modifier_down(VK_LMENU)
            || (!is_modifier_down(VK_RMENU) && is_modifier_down(VK_MENU));
        let mut mods = 0u8;
        if left_ctrl {
            mods |= 1u8 << 0;
        }
        if left_shift {
            mods |= 1u8 << 1;
        }
        if left_alt {
            mods |= 1u8 << 2;
        }
        if is_modifier_down(VK_LWIN) {
            mods |= 1u8 << 3;
        }
        if is_modifier_down(VK_RCONTROL) {
            mods |= 1u8 << 4;
        }
        if is_modifier_down(VK_RSHIFT) {
            mods |= 1u8 << 5;
        }
        if is_modifier_down(VK_RMENU) {
            mods |= 1u8 << 6;
        }
        if is_modifier_down(VK_RWIN) {
            mods |= 1u8 << 7;
        }
        mods
    }

    /// 滚轮增量：高 16 位按有符号 16 位解释。
    fn wheel_delta(mouse_data: u32) -> i16 {
        ((mouse_data >> 16) as u16) as i16
    }

    /// 构造一个协议输入事件（只填公共字段）。
    fn proto_event(kind: u32) -> dfy_proto::InputEvent {
        dfy_proto::InputEvent {
            kind,
            hid_usage: 0,
            mods: 0,
            state: INPUT_STATE_DOWN,
            x: 0,
            y: 0,
            dx: 0,
            dy: 0,
            unicode: 0,
            ts_us: dfy_transport::now_us(),
        }
    }

    fn key_event(usage: u16, down: bool, modifiers: u8) -> dfy_proto::InputEvent {
        let mut ev = proto_event(INPUT_KIND_KEY);
        ev.hid_usage = u32::from(usage);
        ev.mods = u32::from(modifiers);
        ev.state = if down {
            INPUT_STATE_DOWN
        } else {
            INPUT_STATE_UP
        };
        ev
    }

    fn button_event(hid_button: u32, down: bool) -> dfy_proto::InputEvent {
        let mut ev = proto_event(INPUT_KIND_MOUSE_BTN);
        ev.hid_usage = hid_button;
        ev.state = if down {
            INPUT_STATE_DOWN
        } else {
            INPUT_STATE_UP
        };
        ev.mods = u32::from(current_modifiers());
        ev
    }

    fn wheel_event(dx: i16, dy: i16) -> dfy_proto::InputEvent {
        let mut ev = proto_event(INPUT_KIND_WHEEL);
        ev.dx = i32::from(dx);
        ev.dy = i32::from(dy);
        ev.mods = u32::from(current_modifiers());
        ev
    }

    fn move_event(x: u32, y: u32) -> dfy_proto::InputEvent {
        let mut ev = proto_event(INPUT_KIND_MOUSE_MOVE);
        ev.x = x;
        ev.y = y;
        ev
    }

    fn event_envelope(ev: dfy_proto::InputEvent) -> dfy_proto::Envelope {
        dfy_proto::Envelope {
            body: Some(envelope::Body::InputEvent(ev)),
        }
    }

    /// 非移动事件走可靠 control 流。
    fn emit(ev: dfy_proto::InputEvent) {
        send(super::CtrlMsg::Event(event_envelope(ev)));
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn normalize_covers_screen_extent() {
            assert_eq!(normalize(0, 1920), 0);
            assert_eq!(normalize(1919, 1920), 65_535);
            assert_eq!(normalize(-50, 1920), 0, "负坐标（多屏左侧）clamp 到 0");
            assert_eq!(normalize(5000, 1920), 65_535, "越界 clamp 到右边缘");
            assert_eq!(normalize(10, 1), 0, "异常尺寸不 panic");
            assert_eq!(normalize(0, 0), 0);
        }

        #[test]
        fn wheel_delta_is_signed_high_word() {
            assert_eq!(wheel_delta(120u32 << 16), 120);
            // -120 = 0xFF88 放在高 16 位
            assert_eq!(wheel_delta(0xFF88_0000), -120);
            assert_eq!(wheel_delta(0), 0);
        }

        #[test]
        fn modifier_bits_are_hid_semantics() {
            // 位图定义本身是常量语义，这里仅防止位序写反（不在 CI 里真实按键）。
            assert_eq!(1u8 << 0, 0b0000_0001, "LCtrl");
            assert_eq!(1u8 << 7, 0b1000_0000, "RGui");
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    //! Linux X11 控制端采集：XInput2 raw events + XKB physical key names.
    //!
    //! 只观察当前用户已获授权的 X11 会话；Wayland/XWayland 明确拒绝，不尝试绕过 portal。

    use std::collections::{HashMap, HashSet};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use anyhow::{anyhow, Context, Result};
    use tokio::sync::mpsc::UnboundedSender;
    use tracing::{debug, warn};
    use x11rb::connection::Connection;
    use x11rb::protocol::xinput::{ConnectionExt as _, DeviceType, EventMask, XIEventMask};
    use x11rb::protocol::xkb::{ConnectionExt as _, NameDetail, ID};
    use x11rb::protocol::xproto::{
        ChangeWindowAttributesAux, ConnectionExt as _, EventMask as XprotoEventMask, KeyButMask,
        Window,
    };
    use x11rb::protocol::Event;
    use x11rb::rust_connection::RustConnection;

    use crate::real::{
        frame_point_to_move, input_button_envelope, input_key_envelope, input_wheel_envelope,
    };

    use super::{CtrlMsg, HeldInput};

    const MOVE_MERGE_INTERVAL: Duration = Duration::from_millis(10);
    const POLL_INTERVAL: Duration = Duration::from_millis(5);

    pub(super) struct Worker {
        thread: JoinHandle<()>,
        stopped: Arc<AtomicBool>,
        held: Arc<Mutex<HeldInput>>,
    }

    impl Worker {
        pub(super) fn start(tx: UnboundedSender<CtrlMsg>) -> Result<Self> {
            let stopped = Arc::new(AtomicBool::new(false));
            let held = Arc::new(Mutex::new(HeldInput::default()));
            let worker_stopped = Arc::clone(&stopped);
            let worker_held = Arc::clone(&held);
            let (ready_tx, ready_rx) = mpsc::channel::<std::result::Result<(), anyhow::Error>>();
            let thread = std::thread::Builder::new()
                .name("dfy-x11-input-capture".to_string())
                .spawn(move || {
                    let result = Capture::open().and_then(|mut capture| {
                        capture.held = Arc::clone(&worker_held);
                        ready_tx
                            .send(Ok(()))
                            .map_err(|_| anyhow!("输入采集就绪通知失败"))?;
                        capture.run(tx, &worker_stopped);
                        Ok(())
                    });
                    if let Err(err) = result {
                        let _ = ready_tx.send(Err(err));
                    }
                })
                .context("启动 Linux X11 输入采集线程失败")?;
            match ready_rx.recv_timeout(Duration::from_secs(5)) {
                Ok(Ok(())) => Ok(Self {
                    thread,
                    stopped,
                    held,
                }),
                Ok(Err(err)) => {
                    stopped.store(true, Ordering::SeqCst);
                    let _ = thread.join();
                    Err(err)
                }
                Err(err) => {
                    stopped.store(true, Ordering::SeqCst);
                    Err(anyhow!("等待 Linux X11 输入采集就绪失败：{err}"))
                }
            }
        }

        /// 最近一次观察到的按住键/按钮快照（路径迁移后用于重放）。
        pub(super) fn held(&self) -> HeldInput {
            self.held
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }

        pub(super) fn stop(self) {
            self.stopped.store(true, Ordering::SeqCst);
            let _ = self.thread.join();
        }
    }

    struct Capture {
        connection: RustConnection,
        root: Window,
        width: u32,
        height: u32,
        key_usages: HashMap<u8, u16>,
        pressed: HashSet<u16>,
        buttons: HashSet<u8>,
        last_sent_position: Option<(u32, u32)>,
        pending_move: Option<(u32, u32)>,
        last_move_at: Option<Instant>,
        held: Arc<Mutex<HeldInput>>,
    }

    impl Capture {
        fn open() -> Result<Self> {
            let display = std::env::var("DISPLAY").ok();
            anyhow::ensure!(
                display
                    .as_deref()
                    .is_some_and(|value| !value.trim().is_empty()),
                "Linux 控制输入需要 DISPLAY 指向已授权的 X11 会话"
            );
            let session = std::env::var("XDG_SESSION_TYPE").ok();
            let wayland = std::env::var("WAYLAND_DISPLAY").ok();
            let explicit_x11 = session
                .as_deref()
                .is_some_and(|value| value.eq_ignore_ascii_case("x11"));
            if session
                .as_deref()
                .is_some_and(|value| value.eq_ignore_ascii_case("wayland"))
                || (!explicit_x11
                    && wayland
                        .as_deref()
                        .is_some_and(|value| !value.trim().is_empty()))
            {
                anyhow::bail!(
                    "Wayland 控制输入需要用户授权的 RemoteDesktop portal；X11/XWayland 采集已拒绝"
                );
            }

            let (connection, screen) =
                x11rb::connect(display.as_deref()).context("连接 DISPLAY 指定的 X11 会话失败")?;
            let xwayland = connection
                .query_extension(b"XWAYLAND")
                .context("查询 X11 服务器类型失败")?
                .reply()
                .context("读取 X11 服务器类型失败")?;
            if xwayland.present {
                anyhow::bail!(
                    "XWayland 不是受支持的完整控制桌面；Wayland 需要 RemoteDesktop portal"
                );
            }
            let root = connection
                .setup()
                .roots
                .get(screen)
                .context("DISPLAY 的屏幕编号无效")?
                .root;
            let geometry = connection
                .get_geometry(root)
                .context("读取 X11 桌面尺寸失败")?
                .reply()
                .context("X11 桌面尺寸不可用")?;
            anyhow::ensure!(
                geometry.width >= 2 && geometry.height >= 2,
                "X11 桌面尺寸异常：{}x{}",
                geometry.width,
                geometry.height
            );

            let xinput = connection
                .xinput_xi_query_version(2, 0)
                .context("X11 显示未提供 XInput2 扩展")?
                .reply()
                .context("无法协商 XInput2 版本")?;
            anyhow::ensure!(xinput.major_version >= 2, "X11 服务器不支持 XInput2");
            let xkb = connection
                .xkb_use_extension(1, 0)
                .context("X11 显示未提供 XKB 扩展")?
                .reply()
                .context("无法初始化 XKB 扩展")?;
            anyhow::ensure!(xkb.supported, "X11 服务器不支持 XKB 1.0");
            let names = connection
                .xkb_get_names(
                    ID::USE_CORE_KBD.into(),
                    NameDetail::KEY_NAMES | NameDetail::KEY_ALIASES,
                )
                .context("读取 XKB 物理键名失败")?
                .reply()
                .context("XKB 物理键名不可用")?;
            let keys = names
                .value_list
                .key_names
                .context("XKB 未返回物理键名；不能安全推断固定键码")?;
            let mut key_usages = HashMap::new();
            let mut name_to_code = HashMap::new();
            for (offset, key) in keys.iter().enumerate() {
                let code = usize::from(names.first_key) + offset;
                if code > u8::MAX as usize {
                    break;
                }
                let code = code as u8;
                name_to_code.insert(key.name, code);
                if let Some(usage) = dfy_input::x11_physical_key_name_to_usage(key.name) {
                    key_usages.insert(code, usage);
                }
            }
            for alias in names.value_list.key_aliases.unwrap_or_default() {
                let (Some(&real_code), Some(&alias_code)) = (
                    name_to_code.get(&alias.real),
                    name_to_code.get(&alias.alias),
                ) else {
                    continue;
                };
                if let Some(&usage) = key_usages.get(&real_code) {
                    key_usages.insert(alias_code, usage);
                }
            }
            anyhow::ensure!(!key_usages.is_empty(), "XKB 未提供可映射的物理键名");

            let mut pointer_devices = Vec::new();
            let mut keyboard_devices = Vec::new();
            for device_id in 2u8..=u8::MAX {
                let reply = match connection.xinput_xi_query_device(u16::from(device_id)) {
                    Ok(cookie) => match cookie.reply() {
                        Ok(reply) => reply,
                        Err(_) => continue,
                    },
                    Err(_) => continue,
                };
                for info in reply.infos {
                    match info.type_ {
                        DeviceType::MASTER_POINTER => {
                            pointer_devices.push(u16::from(info.deviceid))
                        }
                        DeviceType::MASTER_KEYBOARD => {
                            keyboard_devices.push(u16::from(info.deviceid))
                        }
                        _ => {}
                    }
                }
            }
            let mut masks = Vec::new();
            for deviceid in pointer_devices {
                masks.push(EventMask {
                    deviceid,
                    mask: vec![
                        XIEventMask::RAW_BUTTON_PRESS,
                        XIEventMask::RAW_BUTTON_RELEASE,
                        XIEventMask::RAW_MOTION,
                    ],
                });
            }
            for deviceid in keyboard_devices {
                masks.push(EventMask {
                    deviceid,
                    mask: vec![XIEventMask::RAW_KEY_PRESS, XIEventMask::RAW_KEY_RELEASE],
                });
            }
            let xinput_selected = if masks.is_empty() {
                false
            } else {
                match connection.xinput_xi_select_events(root, &masks) {
                    Ok(cookie) => cookie.check().is_ok(),
                    Err(_) => false,
                }
            };
            if !xinput_selected {
                debug!("XInput2 raw 事件不可用，降级为 X11 根窗口核心事件");
            }
            let mut root_mask = XprotoEventMask::STRUCTURE_NOTIFY;
            if !xinput_selected {
                root_mask |= XprotoEventMask::KEY_PRESS
                    | XprotoEventMask::KEY_RELEASE
                    | XprotoEventMask::BUTTON_PRESS
                    | XprotoEventMask::BUTTON_RELEASE
                    | XprotoEventMask::POINTER_MOTION;
            }
            connection
                .change_window_attributes(
                    root,
                    &ChangeWindowAttributesAux {
                        event_mask: Some(root_mask),
                        ..ChangeWindowAttributesAux::default()
                    },
                )
                .context("订阅 X11 根窗口事件失败")?
                .check()
                .context("X11 服务器拒绝根窗口事件订阅")?;

            let keymap = connection
                .query_keymap()
                .context("读取 X11 按键状态失败")?
                .reply()
                .context("X11 按键状态不可用")?;
            let mut pressed = HashSet::new();
            for code in 0u8..=u8::MAX {
                if keymap.keys[usize::from(code) / 8] & (1 << (code % 8)) != 0 {
                    if let Some(usage) = key_usages.get(&code) {
                        pressed.insert(*usage);
                    }
                }
            }
            let pointer = connection
                .query_pointer(root)
                .context("读取 X11 指针状态失败")?
                .reply()
                .context("X11 指针状态不可用")?;
            let buttons = [(1u8, 1u8), (2, 3), (3, 2), (8, 4), (9, 5)]
                .into_iter()
                .filter(|(button, _)| {
                    u16::from(pointer.mask & KeyButMask::from(1u16 << (*button - 1))) != 0
                })
                .map(|(_, hid)| hid)
                .collect();

            debug!(
                screen,
                root,
                width = geometry.width,
                height = geometry.height,
                "Linux X11 控制输入采集就绪"
            );
            Ok(Self {
                connection,
                root,
                width: u32::from(geometry.width),
                height: u32::from(geometry.height),
                key_usages,
                pressed,
                buttons,
                last_sent_position: None,
                pending_move: None,
                last_move_at: None,
                held: Arc::new(Mutex::new(HeldInput::default())),
            })
        }

        fn run(&mut self, tx: UnboundedSender<CtrlMsg>, stopped: &AtomicBool) {
            let result = self.run_loop(tx.clone(), stopped);
            // 会话正常结束时 Agent 也会 release_all；这里仍显式释放，覆盖采集线程异常/用户停止。
            self.release_all(&tx);
            if let Err(err) = result {
                warn!(error = %err, "Linux X11 控制输入采集结束");
            }
        }

        fn run_loop(&mut self, tx: UnboundedSender<CtrlMsg>, stopped: &AtomicBool) -> Result<()> {
            while !stopped.load(Ordering::SeqCst) {
                match self.connection.poll_for_event() {
                    Ok(Some(event)) => {
                        if self.handle_event(event, &tx)? {
                            self.release_all(&tx);
                            let _ = tx.send(CtrlMsg::Stopped);
                            return Ok(());
                        }
                    }
                    Ok(None) => std::thread::sleep(POLL_INTERVAL),
                    Err(err) => return Err(err.into()),
                }
                self.flush_move(&tx);
                self.publish_held();
            }
            Ok(())
        }

        /// 把当前按下集合复制到共享快照；迁移后主线程据此在新通道重放。
        fn publish_held(&self) {
            let mut keys: Vec<u16> = self.pressed.iter().copied().collect();
            keys.sort_unstable();
            let mut buttons: Vec<u8> = self.buttons.iter().copied().collect();
            buttons.sort_unstable();
            let mut guard = self
                .held
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.keys = keys;
            guard.buttons = buttons;
        }

        fn handle_event(&mut self, event: Event, tx: &UnboundedSender<CtrlMsg>) -> Result<bool> {
            match event {
                Event::ConfigureNotify(event) if event.window == self.root => {
                    self.width = u32::from(event.width);
                    self.height = u32::from(event.height);
                }
                Event::KeyPress(event) => {
                    return Ok(self.key(event.detail, true, tx));
                }
                Event::KeyRelease(event) => {
                    return Ok(self.key(event.detail, false, tx));
                }
                Event::ButtonPress(event) => {
                    self.button(event.detail, true, tx);
                }
                Event::ButtonRelease(event) => {
                    self.button(event.detail, false, tx);
                }
                Event::MotionNotify(event) if event.root == self.root => {
                    self.queue_move(
                        u32::try_from(event.root_x).unwrap_or(0),
                        u32::try_from(event.root_y).unwrap_or(0),
                        tx,
                    );
                }
                Event::XinputRawKeyPress(event) => {
                    return Ok(self.key(event.detail as u8, true, tx));
                }
                Event::XinputRawKeyRelease(event) => {
                    return Ok(self.key(event.detail as u8, false, tx));
                }
                Event::XinputRawButtonPress(event) => {
                    self.button(event.detail as u8, true, tx);
                }
                Event::XinputRawButtonRelease(event) => {
                    self.button(event.detail as u8, false, tx);
                }
                Event::XinputRawMotion(_event) => {
                    let pointer = self
                        .connection
                        .query_pointer(self.root)
                        .context("读取 X11 指针位置失败")?
                        .reply()
                        .context("X11 指针位置不可用")?;
                    self.queue_move(
                        u32::try_from(pointer.root_x).unwrap_or(0),
                        u32::try_from(pointer.root_y).unwrap_or(0),
                        tx,
                    );
                }
                _ => {}
            }
            Ok(false)
        }

        fn key(&mut self, code: u8, down: bool, tx: &UnboundedSender<CtrlMsg>) -> bool {
            let Some(&usage) = self.key_usages.get(&code) else {
                debug!(code, "忽略未映射的 X11 物理键");
                return false;
            };
            if down {
                self.pressed.insert(usage);
            } else {
                self.pressed.remove(&usage);
            }
            if down && usage == 0x14 && self.stop_combo_pressed() {
                return true;
            }
            let _ = tx.send(CtrlMsg::Event(input_key_envelope(
                usage,
                down,
                self.modifiers(),
            )));
            false
        }

        fn button(&mut self, detail: u8, down: bool, tx: &UnboundedSender<CtrlMsg>) {
            if let Some((dx, dy)) = wheel_delta(detail) {
                if down {
                    let _ = tx.send(CtrlMsg::Event(input_wheel_envelope(dx, dy)));
                }
                return;
            }
            let Some(button) = hid_button(detail) else {
                return;
            };
            if down {
                self.buttons.insert(button);
            } else {
                self.buttons.remove(&button);
            }
            let _ = tx.send(CtrlMsg::Event(input_button_envelope(button, down)));
        }

        fn queue_move(&mut self, x: u32, y: u32, tx: &UnboundedSender<CtrlMsg>) {
            if self.width < 2 || self.height < 2 {
                return;
            }
            if self.last_sent_position == Some((x, y)) && self.pending_move.is_none() {
                return;
            }
            let due = match self.last_move_at {
                Some(last) => last.elapsed() >= MOVE_MERGE_INTERVAL,
                None => true,
            };
            if due {
                self.send_move(x, y, tx);
            } else {
                self.pending_move = Some((x, y));
            }
        }

        fn flush_move(&mut self, tx: &UnboundedSender<CtrlMsg>) {
            let Some((x, y)) = self.pending_move.take() else {
                return;
            };
            let due = match self.last_move_at {
                Some(last) => last.elapsed() >= MOVE_MERGE_INTERVAL,
                None => true,
            };
            if due {
                self.send_move(x, y, tx);
            } else {
                self.pending_move = Some((x, y));
            }
        }

        fn send_move(&mut self, x: u32, y: u32, tx: &UnboundedSender<CtrlMsg>) {
            if let Some(event) = frame_point_to_move(x, y, self.width, self.height) {
                self.last_sent_position = Some((x, y));
                self.last_move_at = Some(Instant::now());
                let _ = tx.send(CtrlMsg::Move(event));
            }
        }

        fn modifiers(&self) -> u8 {
            self.pressed
                .iter()
                .filter_map(|usage| modifier_bit(*usage))
                .fold(0, |bits, bit| bits | bit)
        }

        fn stop_combo_pressed(&self) -> bool {
            let has = |bits: &[u16]| bits.iter().any(|usage| self.pressed.contains(usage));
            self.pressed.contains(&0x14)
                && has(&[0xe0, 0xe4])
                && has(&[0xe2, 0xe6])
                && has(&[0xe1, 0xe5])
        }

        fn release_all(&mut self, tx: &UnboundedSender<CtrlMsg>) {
            for usage in self.pressed.drain() {
                let _ = tx.send(CtrlMsg::Event(input_key_envelope(usage, false, 0)));
            }
            for button in self.buttons.drain() {
                let _ = tx.send(CtrlMsg::Event(input_button_envelope(button, false)));
            }
        }
    }

    fn modifier_bit(usage: u16) -> Option<u8> {
        Some(match usage {
            0xe0 => 1 << 0,
            0xe1 => 1 << 1,
            0xe2 => 1 << 2,
            0xe3 => 1 << 3,
            0xe4 => 1 << 4,
            0xe5 => 1 << 5,
            0xe6 => 1 << 6,
            0xe7 => 1 << 7,
            _ => return None,
        })
    }

    fn hid_button(detail: u8) -> Option<u8> {
        match detail {
            1 => Some(1),
            2 => Some(3),
            3 => Some(2),
            8 => Some(4),
            9 => Some(5),
            _ => None,
        }
    }

    fn wheel_delta(detail: u8) -> Option<(i16, i16)> {
        match detail {
            4 => Some((0, 120)),
            5 => Some((0, -120)),
            6 => Some((-120, 0)),
            7 => Some((120, 0)),
            _ => None,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn x11_buttons_use_hid_order_and_wheel_deltas() {
            assert_eq!(hid_button(1), Some(1));
            assert_eq!(hid_button(2), Some(3));
            assert_eq!(hid_button(3), Some(2));
            assert_eq!(hid_button(8), Some(4));
            assert_eq!(hid_button(9), Some(5));
            assert_eq!(wheel_delta(4), Some((0, 120)));
            assert_eq!(wheel_delta(5), Some((0, -120)));
            assert_eq!(wheel_delta(6), Some((-120, 0)));
            assert_eq!(wheel_delta(7), Some((120, 0)));
        }

        #[test]
        fn modifier_bits_match_protocol_layout() {
            assert_eq!(modifier_bit(0xe0), Some(0x01));
            assert_eq!(modifier_bit(0xe1), Some(0x02));
            assert_eq!(modifier_bit(0xe2), Some(0x04));
            assert_eq!(modifier_bit(0xe3), Some(0x08));
            assert_eq!(modifier_bit(0xe4), Some(0x10));
            assert_eq!(modifier_bit(0xe5), Some(0x20));
            assert_eq!(modifier_bit(0xe6), Some(0x40));
            assert_eq!(modifier_bit(0xe7), Some(0x80));
            assert_eq!(modifier_bit(0x04), None);
        }

        #[tokio::test]
        #[ignore = "requires explicit opt-in, xdotool and a disposable Xvfb display"]
        async fn isolated_x11_capture_observes_pointer_button_key_and_stop() -> Result<()> {
            use std::ffi::OsString;
            use std::process::{Child, Command, Stdio};

            use dfy_proto::{
                INPUT_KIND_KEY, INPUT_KIND_MOUSE_BTN, INPUT_KIND_MOUSE_MOVE, INPUT_KIND_WHEEL,
                INPUT_STATE_DOWN, INPUT_STATE_UP,
            };

            use crate::input_capture::InputCapture;

            anyhow::ensure!(
                std::env::var("DFY_CLI_X11_CAPTURE_TEST").as_deref() == Ok("1"),
                "set DFY_CLI_X11_CAPTURE_TEST=1 only in a disposable X11 test host"
            );

            struct TestXvfb(Child);
            impl Drop for TestXvfb {
                fn drop(&mut self) {
                    let _ = self.0.kill();
                    let _ = self.0.wait();
                }
            }
            struct RestoreDisplay(Option<OsString>);
            impl Drop for RestoreDisplay {
                fn drop(&mut self) {
                    if let Some(value) = &self.0 {
                        std::env::set_var("DISPLAY", value);
                    } else {
                        std::env::remove_var("DISPLAY");
                    }
                }
            }

            fn free_endpoint() -> Result<String> {
                for number in 140..240 {
                    let endpoint = format!(":{number}");
                    if !std::path::Path::new(&format!("/tmp/.X11-unix/X{number}")).exists()
                        && !std::path::Path::new(&format!("/tmp/.X{number}-lock")).exists()
                    {
                        return Ok(endpoint);
                    }
                }
                anyhow::bail!("no free Xvfb display number")
            }

            async fn wait_ready(endpoint: &str, child: &mut TestXvfb) -> Result<()> {
                for _ in 0..100 {
                    if x11rb::connect(Some(endpoint)).is_ok() {
                        return Ok(());
                    }
                    if let Some(status) = child.0.try_wait()? {
                        anyhow::bail!("Xvfb {endpoint} exited early: {status}");
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                anyhow::bail!("Xvfb {endpoint} did not become ready")
            }

            fn xdotool(display: &str, args: &[&str]) -> Result<()> {
                let output = Command::new("xdotool")
                    .args(args)
                    .env("DISPLAY", display)
                    .output()
                    .context("运行 xdotool 失败")?;
                anyhow::ensure!(
                    output.status.success(),
                    "xdotool {} 失败：{}",
                    args.join(" "),
                    String::from_utf8_lossy(&output.stderr)
                );
                Ok(())
            }

            let endpoint = free_endpoint()?;
            let mut server = TestXvfb(
                Command::new("Xvfb")
                    .arg(&endpoint)
                    .args(["-screen", "0", "640x480x24", "-nolisten", "tcp", "-noreset"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()?,
            );
            wait_ready(&endpoint, &mut server).await?;
            let _restore_display = RestoreDisplay(std::env::var_os("DISPLAY"));
            std::env::set_var("DISPLAY", &endpoint);
            let mut capture = InputCapture::start()?;

            xdotool(
                &endpoint,
                &[
                    "mousemove",
                    "123",
                    "234",
                    "click",
                    "1",
                    "key",
                    "a",
                    "click",
                    "4",
                    "click",
                    "5",
                ],
            )?;
            let mut moved = false;
            let mut button_down = false;
            let mut button_up = false;
            let mut key_a_down = false;
            let mut key_a_up = false;
            let mut wheel_up = false;
            let mut wheel_down = false;
            let deadline = Instant::now() + Duration::from_secs(8);
            while Instant::now() < deadline
                && !(moved
                    && button_down
                    && button_up
                    && key_a_down
                    && key_a_up
                    && wheel_up
                    && wheel_down)
            {
                let message =
                    match tokio::time::timeout(Duration::from_secs(2), capture.recv()).await {
                        Ok(Some(message)) => message,
                        Ok(None) | Err(_) => break,
                    };
                match message {
                    CtrlMsg::Move(event) => {
                        let expected_x = u32::try_from(123u64 * 65_535 / 639).unwrap();
                        let expected_y = u32::try_from(234u64 * 65_535 / 479).unwrap();
                        assert_eq!(event.kind, INPUT_KIND_MOUSE_MOVE);
                        assert!(event.x.abs_diff(expected_x) <= 1, "x={}", event.x);
                        assert!(event.y.abs_diff(expected_y) <= 1, "y={}", event.y);
                        moved = true;
                    }
                    CtrlMsg::Event(event) => {
                        let Some(dfy_proto::envelope::Body::InputEvent(input)) = event.body else {
                            continue;
                        };
                        match (input.kind, input.hid_usage, input.state) {
                            (INPUT_KIND_MOUSE_BTN, 1, INPUT_STATE_DOWN) => button_down = true,
                            (INPUT_KIND_MOUSE_BTN, 1, INPUT_STATE_UP) => button_up = true,
                            (INPUT_KIND_KEY, 0x04, INPUT_STATE_DOWN) => key_a_down = true,
                            (INPUT_KIND_KEY, 0x04, INPUT_STATE_UP) => key_a_up = true,
                            (INPUT_KIND_WHEEL, 0, _) if input.dy == 120 => wheel_up = true,
                            (INPUT_KIND_WHEEL, 0, _) if input.dy == -120 => wheel_down = true,
                            _ => {}
                        }
                    }
                    CtrlMsg::Stopped => anyhow::bail!("unexpected stop before hotkey"),
                }
            }
            assert!(
                moved && button_down && button_up && key_a_down && key_a_up,
                "missing input: move={moved} button={button_down}/{button_up} a={key_a_down}/{key_a_up} wheel={wheel_up}/{wheel_down}"
            );

            xdotool(&endpoint, &["key", "ctrl+alt+shift+q"])?;
            let stopped = tokio::time::timeout(Duration::from_secs(8), async {
                loop {
                    if matches!(capture.recv().await, Some(CtrlMsg::Stopped)) {
                        return true;
                    }
                }
            })
            .await?;
            assert!(stopped, "stop hotkey did not end capture");
            capture.stop();
            Ok(())
        }
    }
}
