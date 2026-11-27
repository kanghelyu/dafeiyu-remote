// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 输入桥接：把网络上的 `dfy-proto` 输入事件转成 `dfy-input` 注入事件并注入本机。
//!
//! 分层关系（避免依赖倒置）：
//!
//! ```text
//! 控制端 input_capture ──dfy-proto::InputEvent──▶ QUIC ──▶ 被控端 CtrlHandler
//!                                                          │
//!                                                    proto_to_input（本文件）
//!                                                          ▼
//!                                             dfy-input::InputEvent → SendInput
//! ```
//!
//! `dfy-session` 不认识 `dfy-input`，`dfy-input` 也不认识 `dfy-session`；两者只在
//! `dfy-cli` 这一层拼起来，所以转换函数与 [`InjectHandler`] 都放在本 crate。
//!
//! 注意：`dfy-session` 的 [`CtrlHandler`] 方法签名实际是 `&mut self`（不是 `&self`），
//! 且 `on_datagram` 收到的 payload **已去掉通道号首字节**；这里都用 [`std::sync::Mutex`]
//! 保护 sink，并自行把通道号补回去再解码。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, Result};
use dfy_input::{InputEvent, InputSink, KeyState, MouseButton};
use dfy_proto::envelope;
use dfy_proto::{
    CHAN_INPUT_MOVE, INPUT_KIND_KEY, INPUT_KIND_MOUSE_BTN, INPUT_KIND_MOUSE_MOVE, INPUT_KIND_TEXT,
    INPUT_KIND_WHEEL, INPUT_STATE_DOWN, INPUT_STATE_UP,
};
use dfy_session::CtrlHandler;
use tracing::{debug, trace, warn};

/// 把协议层的 [`dfy_proto::InputEvent`] 转成 [`dfy_input::InputEvent`]。
///
/// 字段映射（对齐 DFYP/1 的 `InputEvent` 定义）：
///
/// - `INPUT_KIND_KEY`：`hid_usage` 是 HID Keyboard/Keypad 页（`0x07`）的 Usage ID，
///   直接作为 [`InputEvent::Key::usage`]；`state` 按 `INPUT_STATE_DOWN/UP` 映射；
///   `mods` 低 8 位是 HID 修饰键位图（bit0=LCtrl … bit7=RGui）。
/// - `INPUT_KIND_MOUSE_BTN`：`hid_usage` 是 HID Button 页（`0x0C`）的 1 基编号，
///   `1`=左键、`2`=右键、`3`=中键、`4`=后退（XButton1）、`5`=前进（XButton2）。
/// - `INPUT_KIND_MOUSE_MOVE`：`x`/`y` 是已经归一化到 `0..=65535` 的绝对坐标，
///   直接交给 [`InputEvent::MouseMoveAbs`]（`width`/`height` 取 `65536`，使
///   [`dfy_input`] 的归一化 `x*65535/(extent-1)` 成为恒等变换）。
///   兼容只填 `dx`/`dy` 的对端：当 `x == y == 0` 且 `dx`/`dy` 非零时按相对移动处理。
/// - `INPUT_KIND_WHEEL`：`dx`/`dy` 饱和到 `i16`（单位 Windows `WHEEL_DELTA`）；
///   滚轮没有按下/抬起语义，`state` 忽略。
/// - `INPUT_KIND_TEXT`：`unicode` 码点转 `char`（`0`/非法码点忽略）。
/// - 其它（`TOUCH`/`PAD`/未知 `kind`、非法 `state`、越界按键编号）返回 `None`，
///   由调用方计入「忽略」，不当作错误。
#[must_use]
pub fn proto_to_input(ev: &dfy_proto::InputEvent) -> Option<InputEvent> {
    match ev.kind {
        INPUT_KIND_KEY => {
            let usage = u16::try_from(ev.hid_usage).ok()?;
            if usage == 0 {
                return None; // HID Usage 0 是保留值。
            }
            Some(InputEvent::Key {
                usage,
                state: proto_state(ev.state)?,
                modifiers: (ev.mods & 0xFF) as u8,
            })
        }
        INPUT_KIND_MOUSE_BTN => {
            let button = match ev.hid_usage {
                1 => MouseButton::Left,
                2 => MouseButton::Right,
                3 => MouseButton::Middle,
                4 => MouseButton::Back,
                5 => MouseButton::Forward,
                _ => return None,
            };
            Some(InputEvent::MouseButton {
                button,
                state: proto_state(ev.state)?,
            })
        }
        INPUT_KIND_MOUSE_MOVE => {
            if ev.x == 0 && ev.y == 0 && (ev.dx != 0 || ev.dy != 0) {
                Some(InputEvent::MouseMoveRel {
                    dx: saturate_i16(ev.dx),
                    dy: saturate_i16(ev.dy),
                })
            } else {
                Some(InputEvent::MouseMoveAbs {
                    x: ev.x.min(65_535),
                    y: ev.y.min(65_535),
                    width: 65_536,
                    height: 65_536,
                })
            }
        }
        INPUT_KIND_WHEEL => Some(InputEvent::Wheel {
            dx: saturate_i16(ev.dx),
            dy: saturate_i16(ev.dy),
        }),
        INPUT_KIND_TEXT => {
            let ch = char::from_u32(ev.unicode).filter(|c| *c != '\0')?;
            Some(InputEvent::Text {
                text: ch.to_string(),
            })
        }
        _ => None,
    }
}

/// `u32` 的按下/抬起状态 → [`KeyState`]；非法值返回 `None`。
fn proto_state(state: u32) -> Option<KeyState> {
    match state {
        INPUT_STATE_DOWN => Some(KeyState::Down),
        INPUT_STATE_UP => Some(KeyState::Up),
        _ => None,
    }
}

/// `i32` → `i16` 饱和转换（协议用 `i32`，`dfy-input` 用 `i16`）。
fn saturate_i16(value: i32) -> i16 {
    value.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

/// 一次会话的注入统计（给 agent 侧打印，便于确认事件链是否真的通了）。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct InjectStats {
    /// 成功注入的事件数。
    pub injected: u64,
    /// 收到但无法映射/未启用注入而被忽略的事件数。
    pub ignored: u64,
    /// 注入 API 失败的事件数。
    pub failed: u64,
}

type InputSinkFactory = dyn Fn() -> Result<Box<dyn InputSink>> + Send + Sync;

/// `CtrlHandler` 实现：把控制端发来的输入事件注入本机。
///
/// [`CtrlHandler`] 的方法是 `&mut self`，`serve_agent` 又按值接收 handler；为了能在
/// 会话结束后继续调用 [`InjectHandler::release`] 并读统计，这里以 `Arc` 共享内部状态
/// （`sink` 用 `std::sync::Mutex` 保护——注入是很快的系统调用，不会长时间持锁）。
#[derive(Clone)]
pub struct InjectHandler {
    inner: Arc<InjectInner>,
}

struct InjectInner {
    /// `None` = 禁用注入（未提供 `--allow-input` 或创建后端失败时的降级）。
    sink: Mutex<Option<Box<dyn InputSink>>>,
    reconnect: Mutex<Option<Box<InputSinkFactory>>>,
    /// 迁移后重放按住状态时，重复的 Down 不应二次注入（等价于系统自动重复键）。
    held_keys: Mutex<std::collections::HashSet<u16>>,
    held_buttons: Mutex<std::collections::HashSet<u8>>,
    injected: AtomicU64,
    ignored: AtomicU64,
    failed: AtomicU64,
}

impl InjectHandler {
    /// 创建真正注入本机的处理器（Windows 用 `SendInput`）。
    pub fn new() -> Result<Self> {
        let sink = dfy_input::create_sink().context("创建输入注入后端失败")?;
        Ok(Self::from_sink(Some(sink)))
    }

    /// Creates an input handler bound to one explicit Linux X11 virtual display.
    #[cfg(target_os = "linux")]
    pub fn on_display(display: &str) -> Result<Self> {
        let sink =
            dfy_input::create_sink_on_display(display).context("创建虚拟显示输入注入后端失败")?;
        Ok(Self::from_sink(Some(sink)))
    }

    #[cfg(target_os = "linux")]
    pub fn on_display_shared(display: Arc<Mutex<String>>) -> Result<Self> {
        let endpoint = display
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let sink =
            dfy_input::create_sink_on_display(&endpoint).context("创建虚拟显示输入注入后端失败")?;
        let reconnect_display = Arc::clone(&display);
        Ok(Self::from_sink_with_reconnect(
            Some(sink),
            Some(Box::new(move || {
                let endpoint = reconnect_display
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone();
                dfy_input::create_sink_on_display(&endpoint).context("重连虚拟显示输入注入后端失败")
            })),
        ))
    }

    /// 不注入任何事件的处理器（未提供 `--allow-input` 或注入后端不可用时的降级）。
    #[must_use]
    pub fn disabled() -> Self {
        Self::from_sink(None)
    }

    fn from_sink(sink: Option<Box<dyn InputSink>>) -> Self {
        Self::from_sink_with_reconnect(sink, None)
    }

    fn from_sink_with_reconnect(
        sink: Option<Box<dyn InputSink>>,
        reconnect: Option<Box<InputSinkFactory>>,
    ) -> Self {
        Self {
            inner: Arc::new(InjectInner {
                sink: Mutex::new(sink),
                reconnect: Mutex::new(reconnect),
                held_keys: Mutex::new(std::collections::HashSet::new()),
                held_buttons: Mutex::new(std::collections::HashSet::new()),
                injected: AtomicU64::new(0),
                ignored: AtomicU64::new(0),
                failed: AtomicU64::new(0),
            }),
        }
    }

    /// 是否真的会注入（`false` = 降级/禁用状态）。
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.sink_guard().is_some()
    }

    /// 重复的 Down / 无主 Up 不注入。迁移后重放按住状态时，被控端可能已经处于按下态，
    /// 这里按键/按钮维护按下集合，让重放幂等（同时抑制网络重复包造成的重复按键）。
    /// 返回 `false` 表示该事件被去重，不需要注入。
    fn note_state_change(&self, ev: &InputEvent) -> bool {
        match ev {
            InputEvent::Key { usage, state, .. } => {
                let mut held = self
                    .inner
                    .held_keys
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                match state {
                    KeyState::Down => held.insert(*usage),
                    KeyState::Up => held.remove(usage),
                }
            }
            InputEvent::MouseButton { button, state } => {
                let code = match button {
                    MouseButton::Left => 1,
                    MouseButton::Right => 2,
                    MouseButton::Middle => 3,
                    MouseButton::Back => 4,
                    MouseButton::Forward => 5,
                };
                let mut held = self
                    .inner
                    .held_buttons
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                match state {
                    KeyState::Down => held.insert(code),
                    KeyState::Up => held.remove(&code),
                }
            }
            _ => true,
        }
    }

    /// 注入一个本地事件（带统计与错误吞掉：注入失败不该中断串流会话）。
    pub fn inject(&self, ev: &InputEvent) {
        if !self.note_state_change(ev) {
            return;
        }
        let reconnectable = self
            .inner
            .reconnect
            .lock()
            .map(|factory| factory.is_some())
            .unwrap_or(false);
        match self.inject_once(ev) {
            Some(Ok(())) => {
                self.inner.injected.fetch_add(1, Ordering::Relaxed);
            }
            Some(Err(err)) if reconnectable => {
                debug!(error = %err, "虚拟显示输入连接失效，尝试重连");
                if !self.reconnect() {
                    self.inner.failed.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                match self.inject_once(ev) {
                    Some(Ok(())) => {
                        self.inner.injected.fetch_add(1, Ordering::Relaxed);
                    }
                    _ => {
                        self.inner.failed.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            Some(Err(err)) => {
                self.inner.failed.fetch_add(1, Ordering::Relaxed);
                debug!(error = %err, "输入注入失败");
            }
            None if reconnectable => {
                if self.reconnect() {
                    match self.inject_once(ev) {
                        Some(Ok(())) => {
                            self.inner.injected.fetch_add(1, Ordering::Relaxed);
                        }
                        _ => {
                            self.inner.failed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                } else {
                    self.inner.failed.fetch_add(1, Ordering::Relaxed);
                }
            }
            None => {
                self.inner.ignored.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn inject_once(&self, ev: &InputEvent) -> Option<Result<()>> {
        let mut guard = self.sink_guard();
        guard.as_mut().map(|sink| sink.inject(ev))
    }

    fn reconnect(&self) -> bool {
        let created = {
            let mut factory = match self.inner.reconnect.lock() {
                Ok(factory) => factory,
                Err(poisoned) => poisoned.into_inner(),
            };
            factory.as_mut().and_then(|create| match create() {
                Ok(sink) => Some(sink),
                Err(err) => {
                    warn!(error = %err, "重连虚拟显示输入注入后端失败");
                    None
                }
            })
        };
        let Some(sink) = created else {
            return false;
        };
        *self.sink_guard() = Some(sink);
        true
    }

    /// 释放所有仍处于按下状态的键/鼠标键（会话断开时防卡键）。
    ///
    /// 不依赖 `Drop`：`serve_agent` 结束后调用方需要显式调用一次。
    pub fn release(&self) {
        if let Some(sink) = self.sink_guard().as_mut() {
            sink.release_all();
        }
        self.inner
            .held_keys
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        self.inner
            .held_buttons
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    /// 读取注入统计快照。
    #[must_use]
    pub fn stats(&self) -> InjectStats {
        InjectStats {
            injected: self.inner.injected.load(Ordering::Relaxed),
            ignored: self.inner.ignored.load(Ordering::Relaxed),
            failed: self.inner.failed.load(Ordering::Relaxed),
        }
    }

    /// 加锁取 sink（中毒时取回内部值：注入状态无跨线程不变量，可安全继续）。
    fn sink_guard(&self) -> MutexGuard<'_, Option<Box<dyn InputSink>>> {
        self.inner
            .sink
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl CtrlHandler for InjectHandler {
    fn on_ctrl(&mut self, env: &dfy_proto::Envelope) -> Result<Option<dfy_proto::Envelope>> {
        if let Some(envelope::Body::InputEvent(ev)) = &env.body {
            match proto_to_input(ev) {
                Some(local) => self.inject(&local),
                None => {
                    self.inner.ignored.fetch_add(1, Ordering::Relaxed);
                    trace!(kind = ev.kind, "忽略无法映射的输入事件");
                }
            }
        }
        Ok(None)
    }

    fn on_datagram(&mut self, chan: u8, payload: &[u8]) -> Result<()> {
        if chan != CHAN_INPUT_MOVE {
            return Ok(());
        }
        // `serve_agent` 传进来的 payload 已去掉通道号首字节，而 `decode_input_move`
        // 期望包含它；补回去再解码，避免在 dfy-cli 里引 prost 依赖。
        if payload.is_empty() {
            warn!("收到空的 input-move DATAGRAM");
            return Ok(());
        }
        let mut buf = Vec::with_capacity(payload.len() + 1);
        buf.push(chan);
        buf.extend_from_slice(payload);
        match dfy_proto::decode_input_move(&buf) {
            Ok(ev) => match proto_to_input(&ev) {
                Some(local) => self.inject(&local),
                None => {
                    self.inner.ignored.fetch_add(1, Ordering::Relaxed);
                }
            },
            Err(err) => warn!(error = %err, "解码 input-move DATAGRAM 失败"),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dfy_proto::{
        INPUT_KIND_KEY, INPUT_KIND_MOUSE_BTN, INPUT_KIND_MOUSE_MOVE, INPUT_KIND_PAD,
        INPUT_KIND_TEXT, INPUT_KIND_TOUCH, INPUT_KIND_WHEEL, INPUT_STATE_DOWN, INPUT_STATE_UP,
    };

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires explicit opt-in and disposable Xvfb displays"]
    fn isolated_x11_input_reconnects_to_latest_endpoint() -> anyhow::Result<()> {
        use std::process::{Child, Command, Stdio};
        use x11rb::connection::Connection;
        use x11rb::protocol::xproto::ConnectionExt;

        anyhow::ensure!(
            std::env::var("DFY_CLI_X11_RECOVERY_TEST").as_deref() == Ok("1"),
            "set DFY_CLI_X11_RECOVERY_TEST=1 only in a disposable test host"
        );

        struct TestXvfb(Child);
        impl Drop for TestXvfb {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        fn free_endpoint(exclude: Option<&str>) -> anyhow::Result<String> {
            for number in 100..250 {
                let endpoint = format!(":{number}");
                if Some(endpoint.as_str()) == exclude {
                    continue;
                }
                if !std::path::Path::new(&format!("/tmp/.X11-unix/X{number}")).exists()
                    && !std::path::Path::new(&format!("/tmp/.X{number}-lock")).exists()
                {
                    return Ok(endpoint);
                }
            }
            anyhow::bail!("no free Xvfb display number")
        }

        fn spawn(endpoint: &str) -> anyhow::Result<TestXvfb> {
            Ok(TestXvfb(
                Command::new("Xvfb")
                    .arg(endpoint)
                    .args(["-screen", "0", "640x480x24", "-nolisten", "tcp", "-noreset"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()?,
            ))
        }

        fn wait_ready(endpoint: &str, child: &mut TestXvfb) -> anyhow::Result<()> {
            for _ in 0..100 {
                if let Ok((connection, _)) = x11rb::connect(Some(endpoint)) {
                    drop(connection);
                    return Ok(());
                }
                if let Some(status) = child.0.try_wait()? {
                    anyhow::bail!("Xvfb {endpoint} exited early: {status}");
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            anyhow::bail!("Xvfb {endpoint} did not become ready")
        }

        let old_endpoint = free_endpoint(None)?;
        let mut old_server = spawn(&old_endpoint)?;
        wait_ready(&old_endpoint, &mut old_server)?;
        let live_endpoint = Arc::new(Mutex::new(old_endpoint.clone()));
        let handler = InjectHandler::on_display_shared(Arc::clone(&live_endpoint))?;

        old_server.0.kill()?;
        old_server.0.wait()?;
        let new_endpoint = free_endpoint(Some(&old_endpoint))?;
        let mut new_server = spawn(&new_endpoint)?;
        wait_ready(&new_endpoint, &mut new_server)?;
        *live_endpoint
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = new_endpoint.clone();

        handler.inject(&InputEvent::MouseMoveAbs {
            x: 123,
            y: 234,
            width: 640,
            height: 480,
        });
        let (observer, screen_index) = x11rb::connect(Some(new_endpoint.as_str()))?;
        let root = observer.setup().roots[screen_index].root;
        let pointer = observer.query_pointer(root)?.reply()?;
        assert_eq!((pointer.root_x, pointer.root_y), (123, 234));
        assert_eq!(
            handler.stats(),
            InjectStats {
                injected: 1,
                ignored: 0,
                failed: 0,
            }
        );
        new_server.0.kill()?;
        new_server.0.wait()?;
        Ok(())
    }

    #[test]
    fn failed_virtual_input_reconnects_and_retries_once() {
        struct FailingSink;
        impl InputSink for FailingSink {
            fn inject(&mut self, _: &InputEvent) -> Result<()> {
                anyhow::bail!("display disconnected")
            }

            fn release_all(&mut self) {}
        }

        struct WorkingSink(Arc<AtomicU64>);
        impl InputSink for WorkingSink {
            fn inject(&mut self, _: &InputEvent) -> Result<()> {
                self.0.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }

            fn release_all(&mut self) {}
        }

        let delivered = Arc::new(AtomicU64::new(0));
        let delivered_for_sink = Arc::clone(&delivered);
        let reconnects = Arc::new(AtomicU64::new(0));
        let reconnects_for_factory = Arc::clone(&reconnects);
        let handler = InjectHandler::from_sink_with_reconnect(
            Some(Box::new(FailingSink)),
            Some(Box::new(move || {
                reconnects_for_factory.fetch_add(1, Ordering::Relaxed);
                Ok(Box::new(WorkingSink(Arc::clone(&delivered_for_sink))))
            })),
        );
        handler.inject(&InputEvent::MouseMoveRel { dx: 1, dy: 1 });
        assert_eq!(delivered.load(Ordering::Relaxed), 1);
        assert_eq!(reconnects.load(Ordering::Relaxed), 1);
        assert_eq!(
            handler.stats(),
            InjectStats {
                injected: 1,
                ignored: 0,
                failed: 0,
            }
        );
    }

    #[test]
    fn replayed_key_and_button_down_are_idempotent() {
        struct CountingSink(Arc<AtomicU64>);
        impl InputSink for CountingSink {
            fn inject(&mut self, _: &InputEvent) -> Result<()> {
                self.0.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            fn release_all(&mut self) {}
        }
        let delivered = Arc::new(AtomicU64::new(0));
        let handler =
            InjectHandler::from_sink(Some(Box::new(CountingSink(Arc::clone(&delivered)))));
        let key_down = InputEvent::Key {
            usage: 0x04,
            state: KeyState::Down,
            modifiers: 0,
        };
        let key_up = InputEvent::Key {
            usage: 0x04,
            state: KeyState::Up,
            modifiers: 0,
        };
        handler.inject(&key_down);
        handler.inject(&key_down); // 迁移重放的重复 Down
        assert_eq!(delivered.load(Ordering::Relaxed), 1);
        handler.inject(&key_up);
        handler.inject(&key_up); // 重复 Up
        assert_eq!(delivered.load(Ordering::Relaxed), 2);

        let btn = InputEvent::MouseButton {
            button: MouseButton::Left,
            state: KeyState::Down,
        };
        handler.inject(&btn);
        handler.inject(&btn);
        assert_eq!(delivered.load(Ordering::Relaxed), 3);

        // release() 清空按下集合，重放可以重新按下。
        handler.release();
        handler.inject(&key_down);
        assert_eq!(delivered.load(Ordering::Relaxed), 4);
    }

    /// 构造协议事件的小工具（只填关心字段）。
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
            ts_us: 0,
        }
    }

    #[test]
    fn key_maps_to_hid_usage_and_state() {
        let mut ev = proto_event(INPUT_KIND_KEY);
        ev.hid_usage = 0x04; // A
        ev.mods = 0b0000_0010; // LShift
        ev.state = INPUT_STATE_DOWN;
        assert_eq!(
            proto_to_input(&ev),
            Some(InputEvent::Key {
                usage: 0x04,
                state: KeyState::Down,
                modifiers: 0b0000_0010,
            })
        );

        ev.state = INPUT_STATE_UP;
        assert_eq!(
            proto_to_input(&ev),
            Some(InputEvent::Key {
                usage: 0x04,
                state: KeyState::Up,
                modifiers: 0b0000_0010,
            })
        );

        // 非法 state / usage 0 / 超出 u16 的 usage 都应该被忽略。
        ev.state = 7;
        assert_eq!(proto_to_input(&ev), None);
        ev.state = INPUT_STATE_DOWN;
        ev.hid_usage = 0;
        assert_eq!(proto_to_input(&ev), None);
        ev.hid_usage = 0x1_0000;
        assert_eq!(proto_to_input(&ev), None);
    }

    #[test]
    fn mouse_buttons_map_to_hid_button_page() {
        let mut ev = proto_event(INPUT_KIND_MOUSE_BTN);
        let cases = [
            (1u32, MouseButton::Left),
            (2, MouseButton::Right),
            (3, MouseButton::Middle),
            (4, MouseButton::Back),
            (5, MouseButton::Forward),
        ];
        for (usage, button) in cases {
            ev.hid_usage = usage;
            ev.state = INPUT_STATE_DOWN;
            assert_eq!(
                proto_to_input(&ev),
                Some(InputEvent::MouseButton {
                    button,
                    state: KeyState::Down
                })
            );
        }
        ev.hid_usage = 6; // 未定义按键
        assert_eq!(proto_to_input(&ev), None);
        ev.hid_usage = 1;
        ev.state = INPUT_STATE_UP;
        assert_eq!(
            proto_to_input(&ev),
            Some(InputEvent::MouseButton {
                button: MouseButton::Left,
                state: KeyState::Up
            })
        );
    }

    #[test]
    fn mouse_move_absolute_is_identity_normalization() {
        let mut ev = proto_event(INPUT_KIND_MOUSE_MOVE);
        ev.x = 32_767;
        ev.y = 65_535;
        assert_eq!(
            proto_to_input(&ev),
            Some(InputEvent::MouseMoveAbs {
                x: 32_767,
                y: 65_535,
                width: 65_536,
                height: 65_536,
            })
        );

        // 兼容只给相对位移的对端。
        ev.x = 0;
        ev.y = 0;
        ev.dx = -12;
        ev.dy = 3;
        assert_eq!(
            proto_to_input(&ev),
            Some(InputEvent::MouseMoveRel { dx: -12, dy: 3 })
        );
    }

    #[test]
    fn mouse_move_saturates_i32_to_i16() {
        let mut ev = proto_event(INPUT_KIND_MOUSE_MOVE);
        ev.dx = 100_000;
        ev.dy = -100_000;
        assert_eq!(
            proto_to_input(&ev),
            Some(InputEvent::MouseMoveRel {
                dx: i16::MAX,
                dy: i16::MIN
            })
        );
    }

    #[test]
    fn wheel_and_text_map() {
        let mut ev = proto_event(INPUT_KIND_WHEEL);
        ev.dx = -120;
        ev.dy = 120;
        assert_eq!(
            proto_to_input(&ev),
            Some(InputEvent::Wheel { dx: -120, dy: 120 })
        );

        let mut ev = proto_event(INPUT_KIND_TEXT);
        ev.unicode = u32::from('中');
        assert_eq!(
            proto_to_input(&ev),
            Some(InputEvent::Text {
                text: "中".to_string()
            })
        );
        ev.unicode = 0; // 无字符
        assert_eq!(proto_to_input(&ev), None);
        ev.unicode = 0x11_0000; // 非法码点
        assert_eq!(proto_to_input(&ev), None);
    }

    #[test]
    fn unsupported_kinds_are_ignored() {
        for kind in [0u32, INPUT_KIND_TOUCH, INPUT_KIND_PAD, 99] {
            assert_eq!(proto_to_input(&proto_event(kind)), None);
        }
    }

    #[test]
    fn disabled_handler_counts_and_never_panics() {
        let mut handler = InjectHandler::disabled();
        assert!(!handler.is_enabled());
        let mut ev = proto_event(INPUT_KIND_KEY);
        ev.hid_usage = 0x04;
        let env = dfy_proto::Envelope {
            body: Some(envelope::Body::InputEvent(ev)),
        };
        assert!(handler.on_ctrl(&env).unwrap().is_none());
        assert_eq!(
            handler.stats(),
            InjectStats {
                injected: 0,
                ignored: 1,
                failed: 0
            }
        );
        handler.release(); // 禁用状态也必须能安全调用
    }

    #[test]
    fn datagram_payload_is_decoded_after_channel_byte_is_restored() {
        let mut handler = InjectHandler::disabled();
        let mut ev = proto_event(INPUT_KIND_MOUSE_MOVE);
        ev.x = 12_345;
        ev.y = 5432;
        // 走真实编码路径，确保「补回通道号」这一步与 dfy-proto 一致。
        let mut dgram = dfy_proto::encode_input_move(&ev).unwrap();
        assert_eq!(dgram.first().copied(), Some(CHAN_INPUT_MOVE));
        let chan = dgram.remove(0);
        handler.on_datagram(chan, &dgram).unwrap();
        assert_eq!(handler.stats().ignored, 1);

        // 空 payload / 其它通道不应 panic。
        handler.on_datagram(CHAN_INPUT_MOVE, &[]).unwrap();
        handler.on_datagram(9, &dgram).unwrap();
    }
}
