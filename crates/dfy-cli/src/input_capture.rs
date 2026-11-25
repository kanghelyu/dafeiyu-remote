// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 控制端（View）本地输入采集：Windows 低级钩子 → `dfy-proto` 输入事件。
//!
//! 设计要点：
//!
//! - **[`WH_MOUSE_LL`]/[`WH_KEYBOARD_LL`] 全局钩子**：用 `SetWindowsHookExW` 装在一个
//!   专用 `std::thread` 上，线程里跑 `GetMessageW` 消息泵；钩子回调必须在该线程执行，
//!   否则收不到事件。
//! - **鼠标移动节流**：钩子回调只把最新坐标写进 `pending`，`>= 10ms` 才真正发一条
//!   `INPUT_KIND_MOUSE_MOVE` DATAGRAM；窗口内的多次移动用一次性 `SetTimer` 合并成
//!   一条（只发最新），避免 1000Hz 鼠标把网络打爆。
//! - **停止快捷键 `Ctrl+Alt+Shift+Q`**：钩子检测到就置 `AtomicBool`、发
//!   [`CtrlMsg::Stopped`] 并 `PostQuitMessage` 结束消息泵，同时吞掉该按键（避免本机
//!   收到一次没有配对 Down 的 Q）。主循环收到 `Stopped` 后调用
//!   [`InputCapture::stop`] 收尾，用户不会陷入「被自己钩住、本地没法操作」。
//! - **线程间通信**：钩子线程持有 `tokio::sync::mpsc::UnboundedSender<CtrlMsg>`，
//!   tokio 任务侧用 [`InputCapture::recv`] 在 `select!` 里收；无界通道保证钩子回调
//!   永不阻塞（阻塞钩子会让整个系统的输入卡住）。
//! - 非 Windows 平台只提供「不可用」的降级实现，`--control` 时打印提示并继续只收视频。
//!
//! 目前采集范围：鼠标移动 + 左/右/中/后退/前进键 + 滚轮 + 键盘（扫描码优先，回退
//! VK 表）。文本/触摸/手柄不做（钩子拿不到可靠的 Unicode 文本）。

// Windows 低级钩子实现；其它平台调用点被 cfg 掉，保留类型避免 public API 抖动。
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

/// 控制端输入采集器。
///
/// [`InputCapture::disabled`] 得到的实例不采集任何输入，[`InputCapture::recv`] 永远
/// pending，可直接放进 `tokio::select!` 而不空转。
pub struct InputCapture {
    rx: Option<mpsc::UnboundedReceiver<CtrlMsg>>,
    #[cfg(windows)]
    hooks: Option<win::Hooks>,
}

impl InputCapture {
    /// 启动采集（仅 Windows）。
    ///
    /// 返回时钩子已经装好（钩子线程通过握手确认），失败原因可直接打印给用户。
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
        #[cfg(not(windows))]
        {
            anyhow::bail!("当前平台暂不支持控制输入（仅 Windows 已实现采集）")
        }
    }

    /// 不做任何采集的占位实例（`--control` 未开启或平台不支持时使用）。
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            rx: None,
            #[cfg(windows)]
            hooks: None,
        }
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
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{self, Sender};
    use std::sync::Arc;
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
    use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, VIRTUAL_KEY, VK_CONTROL, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN,
        VK_MENU, VK_Q, VK_RCONTROL, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SHIFT,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, GetMessageW, GetSystemMetrics, KillTimer, PeekMessageW, PostQuitMessage,
        PostThreadMessageW, SetTimer, SetWindowsHookExW, UnhookWindowsHookEx, HC_ACTION, HHOOK,
        KBDLLHOOKSTRUCT, LLKHF_EXTENDED, MSG, MSLLHOOKSTRUCT, PM_NOREMOVE, SM_CXSCREEN,
        SM_CYSCREEN, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN,
        WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL,
        WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER,
        WM_XBUTTONDOWN, WM_XBUTTONUP, XBUTTON1, XBUTTON2,
    };

    use super::CtrlMsg;

    /// 鼠标移动合并窗口：窗口内的多次移动只发最新一条。
    const MOVE_MERGE_INTERVAL: Duration = Duration::from_millis(10);
    /// 合并定时器 ID（线程定时器，`SetTimer(None, ..)`）。
    const TIMER_MERGE_ID: usize = 1;
    /// 合并定时器周期（毫秒），略大于窗口以保证窗口内的移动都能被合并。
    const TIMER_MERGE_MS: u32 = 10;

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
    }

    impl Hooks {
        /// 启动钩子线程；返回时钩子已装好（失败会带原因返回）。
        pub(super) fn start(tx: UnboundedSender<super::CtrlMsg>) -> Result<Self> {
            let stopped = Arc::new(AtomicBool::new(false));
            let (ready_tx, ready_rx) = mpsc::channel::<std::result::Result<u32, String>>();
            let stopped_for_thread = Arc::clone(&stopped);
            let thread = std::thread::Builder::new()
                .name("dfy-input-hooks".to_string())
                .spawn(move || thread_main(tx, stopped_for_thread, ready_tx))
                .context("启动输入钩子线程失败")?;
            match ready_rx.recv_timeout(Duration::from_secs(5)) {
                Ok(Ok(thread_id)) => Ok(Self {
                    thread,
                    thread_id,
                    stopped,
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
                run_message_loop();
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
        HOOK.with(|cell| {
            *cell.borrow_mut() = Some(HookState {
                tx: tx.clone(),
                stopped: Arc::clone(stopped),
                screen_width,
                screen_height,
                last_move: None,
                pending_move: None,
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

    /// 消息泵：只处理合并定时器与 `WM_QUIT`，其余消息不需要转发。
    fn run_message_loop() {
        let mut msg = MSG::default();
        loop {
            // SAFETY: `msg` 是有效的可写 `MSG`；无窗口、线程消息循环。
            let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
            if ret.0 <= 0 {
                break; // 0 = WM_QUIT，-1 = 错误
            }
            if msg.message == WM_TIMER && msg.wParam.0 == TIMER_MERGE_ID {
                let _ = unsafe { KillTimer(None, TIMER_MERGE_ID) };
                flush_pending_move();
            }
        }
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
                emit(button_event(1, true));
                None
            }
            WM_LBUTTONUP => {
                emit(button_event(1, false));
                None
            }
            WM_RBUTTONDOWN => {
                emit(button_event(2, true));
                None
            }
            WM_RBUTTONUP => {
                emit(button_event(2, false));
                None
            }
            WM_MBUTTONDOWN => {
                emit(button_event(3, true));
                None
            }
            WM_MBUTTONUP => {
                emit(button_event(3, false));
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
                emit(button_event(usage, wparam.0 as u32 == WM_XBUTTONDOWN));
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
        emit(key_event(usage, down, current_modifiers()));
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

    /// 请求停止采集：置标志、通知主循环、结束消息泵。
    fn request_stop() {
        let notified = HOOK.with(|cell| {
            let Ok(guard) = cell.try_borrow() else {
                return false;
            };
            let Some(state) = guard.as_ref() else {
                return false;
            };
            if state.stopped.swap(true, Ordering::SeqCst) {
                false
            } else {
                let _ = state.tx.send(super::CtrlMsg::Stopped);
                true
            }
        });
        if notified {
            debug!("收到 Ctrl+Alt+Shift+Q，停止控制输入采集");
            unsafe { PostQuitMessage(0) };
        }
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

    /// 非移动事件走可靠 control 流。
    fn emit(ev: dfy_proto::InputEvent) {
        let env = dfy_proto::Envelope {
            body: Some(envelope::Body::InputEvent(ev)),
        };
        send(super::CtrlMsg::Event(env));
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
