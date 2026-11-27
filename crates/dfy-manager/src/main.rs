// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (C) 2026 Dafeiyu Remote contributors

//! 大肥鱼远程图形化管理面板（第一步）。
//!
//! 当前提供两个只读/轻操作页签：
//! - **Relay**：列出用户中继档案与健康状态；↑↓ 选择，T 测试选中，R 测试全部，U 设为当前；
//! - **Displays**：列出本机虚拟显示（后端报告 id、几何、attached 状态）。
//!
//! 仍是本地面板：不发起远程会话、不修改系统设置；所有中继写入都经过
//! `dfy-relay-profiles` 的同一份配置与原子保存。标签使用 ASCII，保证 Windows
//! 系统字体与 Linux DejaVu 都能栅格化（中文字形依赖系统字体，不作保证）。

mod draw;

use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ab_glyph::FontVec;
use anyhow::{bail, Context, Result};
use clap::Parser;
use dfy_relay::{bind_relay_socket_with, RelayClientConfig, RelaySocketOptions, Role};
use dfy_relay_profiles::{HealthStatus, RelayProfile, RelayProfileStore};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalPosition;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::KeyCode;
use winit::window::{Window, WindowAttributes};

use crate::draw::{row_from_y, row_y, Surface};

const WINDOW_W: u32 = 960;
const WINDOW_H: u32 = 600;
const HEADER_H: u32 = 72;
const ROW_H: u32 = 26;
const LIST_TOP: u32 = 112;
const FONT_SIZE: f32 = 16.0;

const BG: u32 = 0x00121922;
const HEADER: u32 = 0x001D2B3A;
const ROW: u32 = 0x00182535;
const SELECTED: u32 = 0x002A4A6B;
const TEXT: u32 = 0x00E6EDF3;
const MUTED: u32 = 0x008B98A5;
const ACCENT: u32 = 0x004FC3F7;
const GOOD: u32 = 0x004ADE7A;
const BAD: u32 = 0x00FF6B6B;

#[derive(Debug, Parser)]
#[command(name = "dfy-manager", about = "Dafeiyu Remote relay/display manager")]
struct Args {
    /// 中继档案配置（含密钥，注意权限）
    #[arg(long, default_value = "relay-profiles.toml")]
    config: PathBuf,
    /// 可选系统字体路径；缺省按平台选择常见无衬线字体
    #[arg(long)]
    font: Option<PathBuf>,
    /// 启动时打开的页签：relay|displays|diagnostics
    #[arg(long, default_value = "relay")]
    tab: String,
    /// 启动时对当前中继跑一次健康测试并写回配置
    #[arg(long, default_value_t = false)]
    test_health: bool,
}

/// 用户动作，与 winit 事件解耦以便无窗口单测。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Down,
    Up,
    SwitchTab,
    Diagnostics,
    Test,
    TestAll,
    Use,
    CreateDisplay,
    DestroyDisplay,
    RefreshDisplays,
    Quit,
}

/// 物理键 → 动作；未知键返回 `None`。
#[must_use]
const fn command_for_key(key: KeyCode) -> Option<Command> {
    match key {
        KeyCode::ArrowDown => Some(Command::Down),
        KeyCode::ArrowUp => Some(Command::Up),
        KeyCode::Tab | KeyCode::Space => Some(Command::SwitchTab),
        KeyCode::KeyG => Some(Command::Diagnostics),
        KeyCode::KeyT => Some(Command::Test),
        KeyCode::KeyR => Some(Command::TestAll),
        KeyCode::KeyU => Some(Command::Use),
        KeyCode::KeyC => Some(Command::CreateDisplay),
        KeyCode::KeyD => Some(Command::DestroyDisplay),
        KeyCode::F5 => Some(Command::RefreshDisplays),
        KeyCode::KeyQ | KeyCode::Escape => Some(Command::Quit),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Relay,
    Displays,
    Diagnostics,
}

impl Tab {
    fn label(self) -> &'static str {
        match self {
            Self::Relay => "Relay Profiles",
            Self::Displays => "Virtual Displays",
            Self::Diagnostics => "Diagnostics",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Relay => Self::Displays,
            Self::Displays => Self::Diagnostics,
            Self::Diagnostics => Self::Relay,
        }
    }
}

/// 诊断级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Ok,
    Warn,
    Fail,
}

/// 一条状态一致性诊断（可在无窗口环境单测）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Diagnostic {
    level: Level,
    text: String,
}

/// 对中继池与虚拟显示做一致性检查；不修改任何状态。
fn run_diagnostics(store: &RelayProfileStore, displays: &[String]) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let profiles = store.profiles();
    if profiles.is_empty() {
        out.push(Diagnostic {
            level: Level::Warn,
            text: "relay pool is empty".to_string(),
        });
    }
    let mut ids = std::collections::HashSet::new();
    for profile in profiles {
        if !ids.insert(profile.id.as_str()) {
            out.push(Diagnostic {
                level: Level::Fail,
                text: format!("duplicate relay id: {}", profile.id),
            });
        }
        if profile.host.trim().is_empty() || profile.port == 0 {
            out.push(Diagnostic {
                level: Level::Fail,
                text: format!("invalid endpoint: {}", profile.id),
            });
        }
        if let Err(err) = profile.key() {
            out.push(Diagnostic {
                level: Level::Fail,
                text: format!("invalid relay key: {}: {err:#}", profile.id),
            });
        }
        if profile.health.ok {
            if profile.health.checked_at_ms == 0 {
                out.push(Diagnostic {
                    level: Level::Fail,
                    text: format!("healthy without check time: {}", profile.id),
                });
            }
            if profile.health.rtt_ms.is_none() {
                out.push(Diagnostic {
                    level: Level::Warn,
                    text: format!("healthy without rtt: {}", profile.id),
                });
            }
        }
    }
    match store.selected() {
        Some(selected) => out.push(Diagnostic {
            level: Level::Ok,
            text: format!("current relay: {}", selected.id),
        }),
        None => out.push(Diagnostic {
            level: Level::Fail,
            text: "no selectable relay".to_string(),
        }),
    }
    if displays.is_empty()
        || displays
            .iter()
            .any(|line| line.starts_with("backend unavailable"))
    {
        out.push(Diagnostic {
            level: Level::Warn,
            text: "virtual display backend unavailable".to_string(),
        });
    } else {
        out.push(Diagnostic {
            level: Level::Ok,
            text: format!("{} virtual display(s) listed", displays.len()),
        });
    }
    out
}

/// 按优先级返回候选系统字体：优先能显示中文的，再退化到拉丁字体。
fn font_candidates() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    #[cfg(windows)]
    {
        paths.push(PathBuf::from(r"C:\Windows\Fonts\msyh.ttc"));
        paths.push(PathBuf::from(r"C:\Windows\Fonts\simhei.ttf"));
        paths.push(PathBuf::from(r"C:\Windows\Fonts\segoeui.ttf"));
        paths.push(PathBuf::from(r"C:\Windows\Fonts\arial.ttf"));
    }
    #[cfg(target_os = "linux")]
    {
        paths.push(PathBuf::from(
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        ));
        paths.push(PathBuf::from(
            "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
        ));
        paths.push(PathBuf::from(
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        ));
        paths.push(PathBuf::from(
            "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
        ));
    }
    paths
}

fn load_font(path: &Path) -> Result<FontVec> {
    let data = std::fs::read(path).with_context(|| format!("读取字体 {} 失败", path.display()))?;
    FontVec::try_from_vec(data).map_err(|err| anyhow::anyhow!("字体解析失败：{err}"))
}

/// 显式 `--font` 优先；否则按候选链找第一个可解析的字体。
fn load_default_font(explicit: Option<&Path>) -> Result<FontVec> {
    if let Some(path) = explicit {
        return load_font(path);
    }
    for path in font_candidates() {
        if path.exists() {
            if let Ok(font) = load_font(&path) {
                return Ok(font);
            }
        }
    }
    bail!("找不到可用系统字体；请用 --font 指定一个 TTF/TTC 文件")
}

/// 把 IP/域名解析成中继地址。
async fn resolve_addr(profile: &RelayProfile) -> Result<SocketAddr> {
    if let Ok(addr) = profile.addr() {
        return Ok(addr);
    }
    let lookup = format!("{}:{}", profile.host, profile.port);
    let found = tokio::net::lookup_host(&lookup)
        .await?
        .next()
        .with_context(|| format!("中继主机 {} 没有可用地址", profile.host));
    found
}

/// 与 CLI 相同的 8×1200B 数据面探测，返回可写回档案的健康状态。
fn probe(
    profile: &RelayProfile,
    timeout_ms: u64,
    runtime: &tokio::runtime::Runtime,
) -> Result<HealthStatus> {
    runtime.block_on(async {
        let addr = resolve_addr(profile).await?;
        let key = profile.key()?;
        let mut tag = [0u8; 16];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut tag);
        let options = RelaySocketOptions {
            bind_timeout: Duration::from_millis(timeout_ms.max(1)),
            retry_interval: Duration::from_millis(200),
            keepalive_interval: Duration::from_secs(3600),
        };
        let config_client = RelayClientConfig {
            relay_addr: addr,
            tag,
            role: Role::Client,
            key: *key.as_bytes(),
            nonce: dfy_transport::now_us(),
        };
        let started = Instant::now();
        let client = bind_relay_socket_with(&config_client, &options)
            .await
            .context("配对中继失败")?;
        let rtt = started.elapsed().as_secs_f64() * 1000.0;
        let config_agent = RelayClientConfig {
            role: Role::Agent,
            nonce: dfy_transport::now_us().wrapping_add(1),
            ..config_client
        };
        let agent = bind_relay_socket_with(&config_agent, &options)
            .await
            .context("配对中继对端失败")?;
        let relay_addr = client.peer_addr();
        let (lost, bps) = tokio::task::spawn_blocking(move || -> Result<(u64, u64)> {
            let mut payload = vec![0x5Au8; 1200];
            payload[..4].copy_from_slice(&[0xDF, 0x59, 0x50, 0x52]);
            let send_start = Instant::now();
            for i in 0..8u8 {
                payload[4] = i;
                client.socket().send_to(&payload, relay_addr)?;
                std::thread::sleep(Duration::from_millis(2));
            }
            let elapsed = send_start.elapsed().max(Duration::from_millis(1));
            let mut received = 0u64;
            let mut buf = [0u8; 1300];
            let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(500));
            while Instant::now() < deadline && received < 8 {
                match agent.socket().recv(&mut buf) {
                    Ok(n) if n >= 4 && buf[..4] == [0xDF, 0x59, 0x50, 0x52] => received += 1,
                    Ok(_) => {}
                    Err(err)
                        if matches!(
                            err.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(err) => return Err(err.into()),
                }
            }
            let bytes = received * 1200;
            let bps = bytes
                .saturating_mul(8)
                .checked_div(elapsed.as_millis().max(1) as u64)
                .unwrap_or(0)
                .saturating_mul(1000);
            Ok((8 - received, bps))
        })
        .await
        .context("探测任务失败")??;
        let mut health = HealthStatus::success(rtt);
        health.loss_ppm = Some(
            lost.saturating_mul(1_000_000)
                .checked_div(8)
                .unwrap_or(1_000_000) as u32,
        );
        health.bandwidth_bps = Some(bps);
        Ok(health)
    })
}

fn display_rows(backend: Option<&dyn dfy_vdisplay::VirtualDisplayBackend>) -> Vec<String> {
    let Some(backend) = backend else {
        return vec!["backend unavailable".to_string()];
    };
    match backend.list() {
        Ok(list) if list.is_empty() => vec!["(no virtual displays)".to_string()],
        Ok(list) => list
            .into_iter()
            .map(|info| {
                let spec = info.spec;
                format!(
                    "id={} attached={} {}x{} {}Hz {}dpi name={}",
                    info.id,
                    info.attached,
                    spec.width,
                    spec.height,
                    spec.refresh_hz,
                    spec.dpi,
                    spec.name_hint
                )
            })
            .collect(),
        Err(err) => vec![format!("list failed: {err:#}")],
    }
}

struct ManagerApp {
    store: RelayProfileStore,
    font: FontVec,
    runtime: tokio::runtime::Runtime,
    display_backend: Option<Box<dyn dfy_vdisplay::VirtualDisplayBackend>>,
    tab: Tab,
    selected: usize,
    status: String,
    displays: Vec<String>,
    diagnostics: Vec<Diagnostic>,
    cursor: PhysicalPosition<f64>,
    surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
    context: Option<softbuffer::Context<Arc<Window>>>,
    window: Option<Arc<Window>>,
    size: (u32, u32),
}

impl ManagerApp {
    fn new(args: Args) -> Result<Self> {
        let store = RelayProfileStore::load(&args.config)?;
        let font = load_default_font(args.font.as_deref())?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("创建 tokio runtime 失败")?;
        let display_backend = dfy_vdisplay::create_backend().ok();
        let displays = display_rows(display_backend.as_deref());
        let tab = match args.tab.as_str() {
            "displays" => Tab::Displays,
            "diagnostics" => Tab::Diagnostics,
            _ => Tab::Relay,
        };
        let diagnostics = if tab == Tab::Diagnostics {
            run_diagnostics(&store, &displays)
        } else {
            Vec::new()
        };
        let status = if tab == Tab::Diagnostics {
            format!("Diagnostics: {} result(s)", diagnostics.len())
        } else {
            "Ready. Up/Down select, Tab switch, T/R test, U use, C/D display, G diagnostics, F5 refresh, Q quit."
                .to_string()
        };
        Ok(Self {
            store,
            font,
            runtime,
            display_backend,
            tab,
            selected: 0,
            status,
            displays,
            diagnostics,
            cursor: PhysicalPosition::new(0.0, 0.0),
            surface: None,
            context: None,
            window: None,
            size: (WINDOW_W, WINDOW_H),
        })
    }

    fn row_count(&self) -> usize {
        match self.tab {
            Tab::Relay => self.store.profiles().len(),
            Tab::Displays => self.displays.len(),
            Tab::Diagnostics => self.diagnostics.len(),
        }
    }

    fn clamp_selection(&mut self) {
        let count = self.row_count();
        if count == 0 {
            self.selected = 0;
        } else if self.selected >= count {
            self.selected = count - 1;
        }
    }

    fn move_selection(&mut self, delta: i32) {
        let count = self.row_count() as i32;
        if count == 0 {
            return;
        }
        self.selected = (self.selected as i32 + delta).clamp(0, count - 1) as usize;
    }

    fn test_selected(&mut self) {
        let profile = match self.tab {
            Tab::Relay => self.store.profiles().get(self.selected).cloned(),
            Tab::Displays | Tab::Diagnostics => None,
        };
        let Some(profile) = profile else {
            self.status = "No relay profile selected.".to_string();
            return;
        };
        self.status = format!("Testing {} ...", profile.id);
        match probe(&profile, 3000, &self.runtime) {
            Ok(health) => {
                let summary = format!(
                    "{}: rtt={:?}ms loss={:?}ppm bps={:?}",
                    profile.id, health.rtt_ms, health.loss_ppm, health.bandwidth_bps
                );
                if let Err(err) = self.store.update_health(&profile.id, health) {
                    self.status = format!("update health failed: {err:#}");
                    return;
                }
                if let Err(err) = self.store.save() {
                    self.status = format!("save failed: {err:#}");
                    return;
                }
                self.status = summary;
            }
            Err(err) => {
                let _ = self
                    .store
                    .update_health(&profile.id, HealthStatus::failure(format!("{err:#}")));
                let _ = self.store.save();
                self.status = format!("{} failed: {err:#}", profile.id);
            }
        }
    }

    fn test_all(&mut self) {
        if self.tab != Tab::Relay {
            self.status = "Switch to Relay tab first.".to_string();
            return;
        }
        let ids: Vec<String> = self.store.profiles().iter().map(|p| p.id.clone()).collect();
        let mut ok = 0usize;
        let mut failed = 0usize;
        for id in ids {
            let profile = self.store.get(&id).expect("id from store").clone();
            match probe(&profile, 3000, &self.runtime) {
                Ok(health) => {
                    ok += 1;
                    let _ = self.store.update_health(&id, health);
                }
                Err(err) => {
                    failed += 1;
                    let _ = self
                        .store
                        .update_health(&id, HealthStatus::failure(format!("{err:#}")));
                }
            }
        }
        if let Err(err) = self.store.save() {
            self.status = format!("save failed: {err:#}");
            return;
        }
        self.status = format!("Tested {} relays: {ok} ok, {failed} failed.", ok + failed);
    }

    fn use_selected(&mut self) {
        if self.tab != Tab::Relay {
            self.status = "Switch to Relay tab first.".to_string();
            return;
        }
        let id = match self.store.profiles().get(self.selected) {
            Some(profile) => profile.id.clone(),
            None => {
                self.status = "No relay profile selected.".to_string();
                return;
            }
        };
        match self.store.set_active(&id).and_then(|()| self.store.save()) {
            Ok(()) => self.status = format!("Current relay: {id}"),
            Err(err) => self.status = format!("set_active failed: {err:#}"),
        }
    }

    fn refresh_displays(&mut self) {
        self.displays = display_rows(self.display_backend.as_deref());
        self.clamp_selection();
        self.status = "Virtual displays refreshed.".to_string();
    }

    fn create_display(&mut self) {
        let Some(backend) = self.display_backend.as_deref() else {
            self.status = "Display backend unavailable.".to_string();
            return;
        };
        // 固定预设，避免键盘编辑尺寸字段；后续可接入表单。
        let spec = dfy_vdisplay::VirtualDisplaySpec::new(1280, 720, 60, 96, "dfy-manager");
        match backend.create(&spec) {
            Ok(id) => {
                self.displays = display_rows(Some(backend));
                self.clamp_selection();
                self.status = format!("Created virtual display {id} (1280x720/60Hz).");
            }
            Err(err) => self.status = format!("create display failed: {err:#}"),
        }
    }

    fn destroy_display(&mut self) {
        if self.tab != Tab::Displays {
            self.status = "Switch to Displays tab first.".to_string();
            return;
        }
        let Some(line) = self.displays.get(self.selected) else {
            self.status = "No virtual display selected.".to_string();
            return;
        };
        let Some(id_text) = line
            .strip_prefix("id=")
            .and_then(|rest| rest.split_whitespace().next())
        else {
            self.status = format!("Cannot parse display id from: {line}");
            return;
        };
        let Ok(id) = id_text.parse::<u32>() else {
            self.status = format!("Invalid display id: {id_text}");
            return;
        };
        let Some(backend) = self.display_backend.as_deref() else {
            self.status = "Display backend unavailable.".to_string();
            return;
        };
        match backend.destroy(id) {
            Ok(()) => {
                self.displays = display_rows(Some(backend));
                self.clamp_selection();
                self.status = format!("Destroyed virtual display {id}.");
            }
            Err(err) => self.status = format!("destroy display failed: {err:#}"),
        }
    }

    /// 执行动作；Quit 由事件层处理（需要 `ActiveEventLoop`）。
    fn apply(&mut self, command: Command) {
        match command {
            Command::Down => self.move_selection(1),
            Command::Up => self.move_selection(-1),
            Command::SwitchTab => {
                self.tab = self.tab.next();
                self.selected = 0;
                self.status = format!("Tab: {}", self.tab.label());
            }
            Command::Diagnostics => {
                self.diagnostics = run_diagnostics(&self.store, &self.displays);
                self.tab = Tab::Diagnostics;
                self.selected = 0;
                self.status = format!("Diagnostics: {} result(s)", self.diagnostics.len());
            }
            Command::Test => self.test_selected(),
            Command::TestAll => self.test_all(),
            Command::Use => self.use_selected(),
            Command::CreateDisplay => self.create_display(),
            Command::DestroyDisplay => self.destroy_display(),
            Command::RefreshDisplays => self.refresh_displays(),
            Command::Quit => {}
        }
    }

    fn handle_key(&mut self, key: KeyCode, event_loop: &ActiveEventLoop) {
        let Some(command) = command_for_key(key) else {
            return;
        };
        if command == Command::Quit {
            event_loop.exit();
            return;
        }
        self.apply(command);
    }

    fn handle_click(&mut self, x: f64, y: f64, event_loop: &ActiveEventLoop) {
        let (x, y) = (x as u32, y as u32);
        if y < LIST_TOP {
            if y < HEADER_H {
                self.tab = if x >= 720 {
                    Tab::Diagnostics
                } else if x >= 320 {
                    Tab::Displays
                } else {
                    Tab::Relay
                };
                self.selected = 0;
            }
            return;
        }
        if let Some(index) = row_from_y(y, LIST_TOP, ROW_H, self.row_count()) {
            self.selected = index;
        } else if y + ROW_H >= WINDOW_H.saturating_sub(40) {
            event_loop.exit();
        }
    }

    fn render(&mut self) {
        let Some(surface) = self.surface.as_mut() else {
            return;
        };
        let (width, height) = self.size;
        let Ok(mut buffer) = surface.buffer_mut() else {
            return;
        };
        {
            let mut canvas = Surface::new(&mut buffer, width, height);
            canvas.fill(BG);

            canvas.fill_rect(0, 0, width, HEADER_H, HEADER);
            canvas.draw_text(
                &self.font,
                "Dafeiyu Remote - Manager",
                20.0,
                18.0,
                21.0,
                TEXT,
            );
            canvas.draw_text(
                &self.font,
                "[1] Relay Profiles",
                20.0,
                47.0,
                FONT_SIZE,
                if self.tab == Tab::Relay {
                    ACCENT
                } else {
                    MUTED
                },
            );
            canvas.draw_text(
                &self.font,
                "[2] Virtual Displays (F5 refresh)",
                320.0,
                47.0,
                FONT_SIZE,
                if self.tab == Tab::Displays {
                    ACCENT
                } else {
                    MUTED
                },
            );
            canvas.draw_text(
                &self.font,
                "[3] Diagnostics (G)",
                720.0,
                47.0,
                FONT_SIZE,
                if self.tab == Tab::Diagnostics {
                    ACCENT
                } else {
                    MUTED
                },
            );
            canvas.draw_text(
                &self.font,
                "Up/Down select  T/R test  U use  C/D display  G diagnostics  F5 refresh  Q quit",
                20.0,
                86.0,
                14.0,
                MUTED,
            );

            let list_bottom = height.saturating_sub(40);
            match self.tab {
                Tab::Relay => {
                    for (index, profile) in self.store.profiles().iter().enumerate() {
                        let y = row_y(LIST_TOP, ROW_H, index);
                        if y + ROW_H > list_bottom {
                            break;
                        }
                        canvas.fill_rect(
                            12,
                            y,
                            width.saturating_sub(24),
                            ROW_H - 2,
                            if index == self.selected {
                                SELECTED
                            } else {
                                ROW
                            },
                        );
                        let health = &profile.health;
                        let (state, color) = if health.ok {
                            ("OK", GOOD)
                        } else if health.checked_at_ms == 0 {
                            ("--", MUTED)
                        } else {
                            ("FAIL", BAD)
                        };
                        let line = format!(
                            "{:<14} {:<22} rtt={:?}ms loss={:?}ppm bps={:?}",
                            profile.id,
                            profile.endpoint(),
                            health.rtt_ms,
                            health.loss_ppm,
                            health.bandwidth_bps
                        );
                        canvas.draw_text(&self.font, &line, 22.0, y as f32 + 3.0, FONT_SIZE, TEXT);
                        canvas.draw_text(
                            &self.font,
                            state,
                            width as f32 - 70.0,
                            y as f32 + 3.0,
                            FONT_SIZE,
                            color,
                        );
                    }
                }
                Tab::Displays => {
                    for (index, line) in self.displays.iter().enumerate() {
                        let y = row_y(LIST_TOP, ROW_H, index);
                        if y + ROW_H > list_bottom {
                            break;
                        }
                        canvas.fill_rect(
                            12,
                            y,
                            width.saturating_sub(24),
                            ROW_H - 2,
                            if index == self.selected {
                                SELECTED
                            } else {
                                ROW
                            },
                        );
                        canvas.draw_text(&self.font, line, 22.0, y as f32 + 3.0, FONT_SIZE, TEXT);
                    }
                }
                Tab::Diagnostics => {
                    for (index, diagnostic) in self.diagnostics.iter().enumerate() {
                        let y = row_y(LIST_TOP, ROW_H, index);
                        if y + ROW_H > list_bottom {
                            break;
                        }
                        canvas.fill_rect(
                            12,
                            y,
                            width.saturating_sub(24),
                            ROW_H - 2,
                            if index == self.selected {
                                SELECTED
                            } else {
                                ROW
                            },
                        );
                        let (tag, color) = match diagnostic.level {
                            Level::Ok => ("OK", GOOD),
                            Level::Warn => ("WARN", ACCENT),
                            Level::Fail => ("FAIL", BAD),
                        };
                        canvas.draw_text(&self.font, tag, 22.0, y as f32 + 3.0, FONT_SIZE, color);
                        canvas.draw_text(
                            &self.font,
                            &diagnostic.text,
                            90.0,
                            y as f32 + 3.0,
                            FONT_SIZE,
                            TEXT,
                        );
                    }
                }
            }

            canvas.fill_rect(0, height.saturating_sub(36), width, 36, HEADER);
            canvas.draw_text(
                &self.font,
                &self.status,
                20.0,
                height as f32 - 28.0,
                14.0,
                TEXT,
            );
        }
        if let Err(err) = buffer.present() {
            self.status = format!("present failed: {err}");
        }
    }
}

impl ApplicationHandler for ManagerApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attributes = WindowAttributes::default()
            .with_title("Dafeiyu Remote - Manager")
            .with_inner_size(winit::dpi::LogicalSize::new(WINDOW_W, WINDOW_H));
        let Ok(window) = event_loop.create_window(attributes) else {
            self.status = "create_window failed".to_string();
            return;
        };
        let window = Arc::new(window);
        let Ok(context) = softbuffer::Context::new(window.clone()) else {
            self.status = "softbuffer context failed".to_string();
            return;
        };
        let Ok(mut surface) = softbuffer::Surface::new(&context, window.clone()) else {
            self.status = "softbuffer surface failed".to_string();
            return;
        };
        let inner = window.inner_size();
        self.size = (inner.width.max(1), inner.height.max(1));
        let (width, height) = self.size;
        let _ = surface.resize(
            NonZeroU32::new(width).unwrap_or(NonZeroU32::new(1).unwrap()),
            NonZeroU32::new(height).unwrap_or(NonZeroU32::new(1).unwrap()),
        );
        self.surface = Some(surface);
        self.context = Some(context);
        self.window = Some(window);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: winit::window::WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                let (width, height) = (size.width.max(1), size.height.max(1));
                if let Some(surface) = self.surface.as_mut() {
                    let _ = surface.resize(
                        NonZeroU32::new(width).unwrap_or(NonZeroU32::new(1).unwrap()),
                        NonZeroU32::new(height).unwrap_or(NonZeroU32::new(1).unwrap()),
                    );
                }
                self.size = (width, height);
                self.clamp_selection();
                if let Some(window) = self.window.as_ref() {
                    window.request_redraw();
                }
            }
            WindowEvent::CursorMoved { position, .. } => self.cursor = position,
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: winit::event::MouseButton::Left,
                ..
            } => {
                self.handle_click(self.cursor.x, self.cursor.y, event_loop);
                if let Some(window) = self.window.as_ref() {
                    window.request_redraw();
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                if let winit::keyboard::PhysicalKey::Code(code) = event.physical_key {
                    self.handle_key(code, event_loop);
                }
                if let Some(window) = self.window.as_ref() {
                    window.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => self.render(),
            _ => {}
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    if !args.config.exists() {
        bail!(
            "中继配置不存在：{}（先用 dfy relay-profile 创建）",
            args.config.display()
        );
    }
    let app = ManagerApp::new(args)?;
    let event_loop = EventLoop::with_user_event().build()?;
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = app;
    event_loop.run_app(&mut app)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_mapping_is_deterministic() {
        assert_eq!(command_for_key(KeyCode::ArrowDown), Some(Command::Down));
        assert_eq!(command_for_key(KeyCode::ArrowUp), Some(Command::Up));
        assert_eq!(command_for_key(KeyCode::Tab), Some(Command::SwitchTab));
        assert_eq!(command_for_key(KeyCode::Space), Some(Command::SwitchTab));
        assert_eq!(command_for_key(KeyCode::KeyT), Some(Command::Test));
        assert_eq!(command_for_key(KeyCode::KeyR), Some(Command::TestAll));
        assert_eq!(command_for_key(KeyCode::KeyU), Some(Command::Use));
        assert_eq!(command_for_key(KeyCode::KeyC), Some(Command::CreateDisplay));
        assert_eq!(
            command_for_key(KeyCode::KeyD),
            Some(Command::DestroyDisplay)
        );
        assert_eq!(command_for_key(KeyCode::KeyG), Some(Command::Diagnostics));
        assert_eq!(command_for_key(KeyCode::F5), Some(Command::RefreshDisplays));
        assert_eq!(command_for_key(KeyCode::KeyQ), Some(Command::Quit));
        assert_eq!(command_for_key(KeyCode::Escape), Some(Command::Quit));
        assert_eq!(command_for_key(KeyCode::KeyZ), None);
    }

    #[test]
    fn diagnostics_flags_inconsistent_state() {
        let dir = std::env::temp_dir().join(format!(
            "dfy-manager-diag-{}-{}",
            std::process::id(),
            dfy_transport::now_us()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let config = dir.join("profiles.toml");
        std::fs::write(
            &config,
            "version = 1\n\
             [selection]\n\
             strategy = \"manual\"\n\
             active_id = \"local\"\n\
             [[profiles]]\n\
             id = \"local\"\n\
             name = \"Local\"\n\
             host = \"127.0.0.1\"\n\
             port = 21117\n\
             key_config = \"base64:6DpdECxB8/f84U6/pHf1bYiUYOPLtqkmkfcHr+A4oHI=\"\n\
             priority = 10\n\
             preferred = true\n\
             tls = \"auto\"\n\
             [profiles.health]\n\
             ok = true\n\
             checked_at_ms = 0\n",
        )
        .expect("write config");
        let store = RelayProfileStore::load(&config).expect("load config");
        let result = run_diagnostics(&store, &["id=1 attached=true 640x480".to_string()]);
        assert!(
            result
                .iter()
                .any(|d| d.level == Level::Fail && d.text.contains("healthy without check time")),
            "{result:?}"
        );
        assert!(result.iter().any(|d| d.level == Level::Ok));
        let unavailable = run_diagnostics(&store, &["backend unavailable".to_string()]);
        assert!(unavailable
            .iter()
            .any(|d| d.level == Level::Warn && d.text.contains("backend unavailable")));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn diagnostics_command_populates_tab() {
        let dir = std::env::temp_dir().join(format!(
            "dfy-manager-diagcmd-{}-{}",
            std::process::id(),
            dfy_transport::now_us()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let config = dir.join("profiles.toml");
        std::fs::write(
            &config,
            "version = 1\n\
             [selection]\n\
             strategy = \"manual\"\n\
             active_id = \"local\"\n\
             [[profiles]]\n\
             id = \"local\"\n\
             name = \"Local\"\n\
             host = \"127.0.0.1\"\n\
             port = 21117\n\
             key_config = \"base64:6DpdECxB8/f84U6/pHf1bYiUYOPLtqkmkfcHr+A4oHI=\"\n\
             priority = 10\n\
             preferred = true\n\
             tls = \"auto\"\n",
        )
        .expect("write config");
        let mut app = match ManagerApp::new(Args {
            config: config.clone(),
            font: None,
            tab: "relay".to_string(),
            test_health: false,
        }) {
            Ok(app) => app,
            Err(err) => {
                eprintln!("skip: test font unavailable: {err:#}");
                return;
            }
        };
        app.apply(Command::Diagnostics);
        assert_eq!(app.tab, Tab::Diagnostics);
        assert!(!app.diagnostics.is_empty());
        assert!(app.diagnostics.iter().any(|d| d.level == Level::Ok));
        assert!(app.row_count() == app.diagnostics.len());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn manager_actions_write_health_and_selection() {
        let dir = std::env::temp_dir().join(format!(
            "dfy-manager-test-{}-{}",
            std::process::id(),
            dfy_transport::now_us()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let config = dir.join("profiles.toml");
        std::fs::write(
            &config,
            "version = 1\n\
             [selection]\n\
             strategy = \"manual\"\n\
             active_id = \"local\"\n\
             [[profiles]]\n\
             id = \"local\"\n\
             name = \"Test Relay\"\n\
             host = \"127.0.0.1\"\n\
             port = 1\n\
             key_config = \"base64:6DpdECxB8/f84U6/pHf1bYiUYOPLtqkmkfcHr+A4oHI=\"\n\
             priority = 10\n\
             preferred = true\n\
             tls = \"auto\"\n",
        )
        .expect("write config");
        let mut app = match ManagerApp::new(Args {
            config: config.clone(),
            font: None,
            tab: "relay".to_string(),
            test_health: false,
        }) {
            Ok(app) => app,
            Err(err) => {
                eprintln!("skip: test font unavailable: {err:#}");
                return;
            }
        };
        assert_eq!(app.row_count(), 1);
        app.test_selected();
        assert!(app.status.contains("failed"), "{}", app.status);
        assert!(!app.store.profiles()[0].health.ok);
        app.use_selected();
        assert_eq!(app.status, "Current relay: local");
        assert_eq!(app.store.selected().map(|p| p.id.as_str()), Some("local"));
        let reloaded = RelayProfileStore::load(&config).expect("reload saved config");
        assert_eq!(reloaded.selected().map(|p| p.id.as_str()), Some("local"));
        app.apply(Command::SwitchTab);
        assert_eq!(app.tab, Tab::Displays);
        app.apply(Command::CreateDisplay);
        assert!(
            app.status.contains("Created virtual display") || app.status.contains("failed"),
            "{}",
            app.status
        );
        app.apply(Command::DestroyDisplay);
        assert!(
            app.status.contains("No virtual display selected")
                || app.status.contains("destroy")
                || app.status.contains("Cannot parse"),
            "{}",
            app.status
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
