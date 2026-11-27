// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! Linux X11/XTest 输入后端（纯 Rust 协议连接，不需要 libXtst 开发库）。
//!
//! 只使用当前进程 DISPLAY/Xauthority 授权。Wayland 会话明确拒绝，不能借 XWayland
//! 提供跨应用输入。物理键码来自服务器 XKB 键名而非 keysym/固定 evdev 偏移。
//! X11 没有通用的直接 Unicode/IME 提交接口；Text 返回明确错误，避免全局改键盘映射。

use crate::x11_common::{require_x11_session, PhysicalKeymap, X11Driver, X11Event, X11Sink};
use crate::{InputError, InputSink, KeyState};
use x11rb::connection::Connection;
use x11rb::protocol::xkb::{ConnectionExt as _, NameDetail, ID};
use x11rb::protocol::xproto::{
    ConnectionExt as _, Window, BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, KEY_PRESS_EVENT,
    KEY_RELEASE_EVENT, MOTION_NOTIFY_EVENT,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::{CURRENT_TIME, NONE};

struct ConnectionDriver {
    connection: RustConnection,
    root: Window,
    keymap: PhysicalKeymap,
}

impl ConnectionDriver {
    fn connect() -> anyhow::Result<Self> {
        let display = std::env::var("DISPLAY").ok();
        Self::connect_display(display.as_deref(), true)
    }

    fn connect_display(
        display: Option<&str>,
        require_desktop_session: bool,
    ) -> anyhow::Result<Self> {
        if require_desktop_session {
            let session = std::env::var("XDG_SESSION_TYPE").ok();
            let wayland = std::env::var("WAYLAND_DISPLAY").ok();
            require_x11_session(session.as_deref(), wayland.as_deref(), display)?;
        } else {
            anyhow::ensure!(
                display.is_some_and(|value| !value.trim().is_empty()),
                "X11 display endpoint 不能为空"
            );
        }
        let (connection, screen) = x11rb::connect(display).map_err(|err| {
            InputError::Unavailable(format!(
                "无法连接 DISPLAY 指定的 X11 会话：{err}；请检查 DISPLAY 与当前用户 Xauthority 授权"
            ))
        })?;
        let xwayland = connection
            .query_extension(b"XWAYLAND")
            .map_err(|err| InputError::Unavailable(format!("查询 X11 服务器类型失败：{err}")))?
            .reply()
            .map_err(|err| InputError::Unavailable(format!("读取 X11 服务器类型失败：{err}")))?;
        if xwayland.present {
            return Err(InputError::UnsupportedSession(
                "XWayland 不是受支持的被控桌面；Wayland 输入需要用户授权的 RemoteDesktop portal"
                    .into(),
            )
            .into());
        }
        let root = connection
            .setup()
            .roots
            .get(screen)
            .ok_or_else(|| InputError::Unavailable("DISPLAY 的屏幕编号无效".into()))?
            .root;
        connection
            .xtest_get_version(2, 2)
            .map_err(|err| InputError::Unavailable(format!("X11 显示未提供 XTest 扩展：{err}")))?
            .reply()
            .map_err(|err| InputError::Unavailable(format!("无法使用 XTest 扩展：{err}")))?;
        let xkb = connection
            .xkb_use_extension(1, 0)
            .map_err(|err| InputError::Unavailable(format!("X11 显示未提供 XKB 扩展：{err}")))?
            .reply()
            .map_err(|err| InputError::Unavailable(format!("无法初始化 XKB 扩展：{err}")))?;
        if !xkb.supported {
            return Err(InputError::Unavailable("X11 服务器不支持 XKB 1.0".into()).into());
        }
        let names = connection
            .xkb_get_names(
                ID::USE_CORE_KBD.into(),
                NameDetail::KEY_NAMES | NameDetail::KEY_ALIASES,
            )
            .map_err(|err| InputError::Unavailable(format!("读取 XKB 物理键名失败：{err}")))?
            .reply()
            .map_err(|err| InputError::Unavailable(format!("XKB 物理键名不可用：{err}")))?;
        let keys = names.value_list.key_names.ok_or_else(|| {
            InputError::Unavailable("XKB 未返回物理键名；不能安全推断固定键码".into())
        })?;
        let aliases = names.value_list.key_aliases.unwrap_or_default();
        let keymap = PhysicalKeymap::new(
            names.first_key,
            &keys.iter().map(|key| key.name).collect::<Vec<_>>(),
            &aliases
                .iter()
                .map(|alias| (alias.real, alias.alias))
                .collect::<Vec<_>>(),
        )?;
        tracing::debug!(root, screen, "dfy-input Linux X11/XTest 注入后端就绪");
        Ok(Self {
            connection,
            root,
            keymap,
        })
    }
}

impl X11Driver for ConnectionDriver {
    fn keycode(&mut self, usage: u16) -> anyhow::Result<u8> {
        self.keymap.keycode(usage)
    }

    fn geometry(&mut self) -> anyhow::Result<(u32, u32)> {
        // 每次读取根窗口实际尺寸，支持 RandR 运行时调整显示分辨率。
        let geometry = self
            .connection
            .get_geometry(self.root)
            .map_err(|err| InputError::Send(format!("读取 X11 桌面尺寸失败：{err}")))?
            .reply()
            .map_err(|err| InputError::Send(format!("X11 桌面尺寸不可用：{err}")))?;
        Ok((u32::from(geometry.width), u32::from(geometry.height)))
    }

    fn send(&mut self, events: &[X11Event]) -> anyhow::Result<()> {
        let mut cookies = Vec::with_capacity(events.len());
        for event in events {
            let (kind, detail, root, x, y) = match *event {
                X11Event::Key { code, state } => (
                    if state == KeyState::Down {
                        KEY_PRESS_EVENT
                    } else {
                        KEY_RELEASE_EVENT
                    },
                    code,
                    NONE,
                    0,
                    0,
                ),
                X11Event::Button { button, state } => (
                    if state == KeyState::Down {
                        BUTTON_PRESS_EVENT
                    } else {
                        BUTTON_RELEASE_EVENT
                    },
                    button,
                    NONE,
                    0,
                    0,
                ),
                // XTest MotionNotify.detail=1 表示相对移动，0 表示根窗口绝对坐标。
                X11Event::Motion { relative, x, y } => (
                    MOTION_NOTIFY_EVENT,
                    u8::from(relative),
                    if relative { NONE } else { self.root },
                    x,
                    y,
                ),
            };
            cookies.push(
                self.connection
                    .xtest_fake_input(kind, detail, CURRENT_TIME, root, x, y, 0)
                    .map_err(|err| InputError::Send(format!("发送 XTest 输入请求失败：{err}")))?,
            );
        }
        // check 会刷新请求并等待服务器验证，不能把仅写入 socket 当作注入成功。
        // 即使一项失败也检查剩余 cookie，避免其他请求的错误被静默丢弃。
        let mut failure = None;
        for cookie in cookies {
            if let Err(err) = cookie.check() {
                if failure.is_none() {
                    failure = Some(InputError::Send(format!(
                        "XTest 输入请求被拒绝：{err}（请检查显示会话授权/服务器策略）"
                    )));
                }
            }
        }
        match failure {
            Some(err) => Err(err.into()),
            None => Ok(()),
        }
    }
}

pub(crate) fn create_sink() -> anyhow::Result<Box<dyn InputSink>> {
    Ok(Box::new(X11Sink::new(ConnectionDriver::connect()?)))
}

pub(crate) fn create_sink_on_display(display: &str) -> anyhow::Result<Box<dyn InputSink>> {
    Ok(Box::new(X11Sink::new(ConnectionDriver::connect_display(
        Some(display),
        false,
    )?)))
}
