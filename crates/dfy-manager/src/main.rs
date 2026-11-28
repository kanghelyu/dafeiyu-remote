// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::fs;
use std::io::{self, BufRead, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use clap::Parser;
use dfy_relay::{bind_relay_socket_with, RelayClientConfig, RelaySocketOptions, Role};
use dfy_relay_profiles::{
    HealthStatus, RelayProfile, RelayProfileStore, SelectionStrategy, TlsMode,
};
use dfy_vdisplay::{
    capability, create_backend, VirtualDisplayBackend, VirtualDisplayInfo, VirtualDisplaySpec,
};
use eframe::egui::{
    self, CentralPanel, Color32, FontDefinitions, FontFamily, IconData, RichText, ScrollArea, Vec2,
};
use rand::{distributions::Alphanumeric, Rng, RngCore};
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

const APP_TITLE: &str = "大肥鱼远程";
const MIN_PASSWORD_BYTES: usize = 16;
const MAX_PASSWORD_BYTES: usize = 1024;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug, Parser)]
#[command(name = "dfy-manager", version, about = "大肥鱼远程桌面管理端")]
struct Args {
    #[arg(long, default_value = "dfy-relay-profiles.toml", value_name = "FILE")]
    config: PathBuf,
    #[arg(long, default_value = "dfy-devices.toml", value_name = "FILE")]
    devices: PathBuf,
    #[arg(long, value_name = "FILE")]
    dfy: Option<PathBuf>,
    #[arg(long, value_name = "TAB")]
    tab: Option<String>,
    #[arg(long, value_name = "FILE")]
    font: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Devices,
    Agent,
    Relays,
    Displays,
    Diagnostics,
}

impl Page {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "devices" | "device" | "d" => Some(Self::Devices),
            "agent" | "allow" | "a" => Some(Self::Agent),
            "relays" | "relay" | "r" => Some(Self::Relays),
            "displays" | "display" | "v" => Some(Self::Displays),
            "diagnostics" | "diagnostic" | "g" => Some(Self::Diagnostics),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Devices => "设备",
            Self::Agent => "允许控制",
            Self::Relays => "中继",
            Self::Displays => "虚拟显示",
            Self::Diagnostics => "诊断",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Self::Devices => "▣",
            Self::Agent => "◉",
            Self::Relays => "⇄",
            Self::Displays => "▤",
            Self::Diagnostics => "✓",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeviceEntry {
    pub id: String,
    pub name: String,
    pub addr: String,
    #[serde(default)]
    pub favorite: bool,
    #[serde(default)]
    pub use_relay: bool,
    #[serde(default)]
    pub relay_tag: String,
    #[serde(default)]
    pub fingerprint: String,
    #[serde(default)]
    pub last_used_ms: u64,
}

impl DeviceEntry {
    fn new(name: String, addr: String) -> Self {
        Self {
            id: format!("device-{}-{}", now_ms(), random_suffix(6)),
            name,
            addr,
            favorite: false,
            use_relay: false,
            relay_tag: String::new(),
            fingerprint: String::new(),
            last_used_ms: 0,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct DeviceBookFile {
    #[serde(default = "one")]
    version: u32,
    #[serde(default)]
    devices: Vec<DeviceEntry>,
}

fn one() -> u32 {
    1
}

#[derive(Debug, Clone)]
struct DeviceBook {
    path: PathBuf,
    devices: Vec<DeviceEntry>,
}

impl DeviceBook {
    fn load(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if !path.exists() {
            return Ok(Self {
                path,
                devices: Vec::new(),
            });
        }
        let text = fs::read_to_string(&path)
            .with_context(|| format!("读取设备列表失败：{}", path.display()))?;
        let file: DeviceBookFile = toml::from_str(&text)
            .with_context(|| format!("解析设备列表失败：{}", path.display()))?;
        if file.version != 1 {
            bail!("不支持的设备列表版本 {}", file.version);
        }
        let mut book = Self {
            path,
            devices: file.devices,
        };
        book.sort();
        Ok(book)
    }

    fn get(&self, id: &str) -> Option<&DeviceEntry> {
        self.devices.iter().find(|entry| entry.id == id)
    }

    fn get_mut(&mut self, id: &str) -> Option<&mut DeviceEntry> {
        self.devices.iter_mut().find(|entry| entry.id == id)
    }

    fn upsert(&mut self, entry: DeviceEntry) {
        if let Some(current) = self.devices.iter_mut().find(|item| item.id == entry.id) {
            *current = entry;
        } else {
            self.devices.push(entry);
        }
        self.sort();
    }

    fn remove(&mut self, id: &str) -> Result<()> {
        let index = self
            .devices
            .iter()
            .position(|entry| entry.id == id)
            .with_context(|| format!("找不到设备：{id}"))?;
        self.devices.remove(index);
        Ok(())
    }

    fn touch(&mut self, id: &str) -> Result<()> {
        let entry = self
            .devices
            .iter_mut()
            .find(|entry| entry.id == id)
            .with_context(|| format!("找不到设备：{id}"))?;
        entry.last_used_ms = now_ms();
        self.sort();
        Ok(())
    }

    fn sort(&mut self) {
        self.devices.sort_by(|a, b| {
            b.favorite
                .cmp(&a.favorite)
                .then(b.last_used_ms.cmp(&a.last_used_ms))
                .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                .then(a.id.cmp(&b.id))
        });
    }

    fn save(&self) -> Result<()> {
        let file = DeviceBookFile {
            version: 1,
            devices: self.devices.clone(),
        };
        let text = toml::to_string_pretty(&file).context("序列化设备列表失败")?;
        atomic_write(&self.path, text.as_bytes())
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("创建配置目录失败：{}", parent.display()))?;
    let temp = path.with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&temp, bytes).with_context(|| format!("写入临时配置失败：{}", temp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temp, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("设置配置权限失败：{}", temp.display()))?;
    }
    fs::rename(&temp, path)
        .with_context(|| format!("替换配置失败：{} -> {}", temp.display(), path.display()))?;
    Ok(())
}

struct Secret(String);

impl Secret {
    fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    fn get(&self) -> &str {
        &self.0
    }

    fn get_mut(&mut self) -> &mut String {
        &mut self.0
    }

    fn clear(&mut self) {
        self.0.zeroize();
        self.0.clear();
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

#[derive(Clone)]
struct ViewerRequest {
    label: String,
    addr: String,
    use_relay: bool,
    relay_tag: String,
    fingerprint: String,
    clipboard: bool,
    device_id: Option<String>,
}

#[derive(Default)]
struct ConnectForm {
    name: String,
    addr: String,
    password: Secret,
    use_relay: bool,
    relay_tag: String,
    fingerprint: String,
    clipboard: bool,
    show_password: bool,
}

impl Default for Secret {
    fn default() -> Self {
        Self::new(String::new())
    }
}

impl ConnectForm {
    fn with_addr(addr: &str) -> Self {
        Self {
            addr: addr.to_string(),
            ..Self::default()
        }
    }
}

struct AgentForm {
    host: String,
    port: String,
    password: Secret,
    show_password: bool,
    allow_input: bool,
    allow_clipboard: bool,
    audio_out: bool,
    use_relay: bool,
    relay_tag: String,
    virtual_displays: u32,
    virtual_monitor: u32,
}

impl Default for AgentForm {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".into(),
            port: "21116".into(),
            password: Secret::default(),
            show_password: false,
            allow_input: true,
            allow_clipboard: false,
            audio_out: false,
            use_relay: false,
            relay_tag: String::new(),
            virtual_displays: 0,
            virtual_monitor: 0,
        }
    }
}

#[derive(Default)]
struct DeviceForm {
    id: Option<String>,
    name: String,
    addr: String,
    favorite: bool,
    use_relay: bool,
    relay_tag: String,
    fingerprint: String,
}

impl DeviceForm {
    fn from_entry(entry: &DeviceEntry) -> Self {
        Self {
            id: Some(entry.id.clone()),
            name: entry.name.clone(),
            addr: entry.addr.clone(),
            favorite: entry.favorite,
            use_relay: entry.use_relay,
            relay_tag: entry.relay_tag.clone(),
            fingerprint: entry.fingerprint.clone(),
        }
    }
}

struct RelayForm {
    id: Option<String>,
    profile_id: String,
    name: String,
    host: String,
    port: String,
    key: Secret,
    region: String,
    priority: String,
    preferred: bool,
    tls: TlsMode,
}

impl Default for RelayForm {
    fn default() -> Self {
        Self {
            id: None,
            profile_id: String::new(),
            name: String::new(),
            host: String::new(),
            port: "21117".into(),
            key: Secret::default(),
            region: String::new(),
            priority: "100".into(),
            preferred: false,
            tls: TlsMode::Auto,
        }
    }
}

impl RelayForm {
    fn from_profile(profile: &RelayProfile) -> Self {
        Self {
            id: Some(profile.id.clone()),
            profile_id: profile.id.clone(),
            name: profile.name.clone(),
            host: profile.host.clone(),
            port: profile.port.to_string(),
            key: Secret::default(),
            region: profile.region.clone().unwrap_or_default(),
            priority: profile.priority.to_string(),
            preferred: profile.preferred,
            tls: profile.tls,
        }
    }
}

struct DisplayDraft {
    width: String,
    height: String,
    refresh: String,
    dpi: String,
    name: String,
}

impl Default for DisplayDraft {
    fn default() -> Self {
        Self {
            width: "1920".into(),
            height: "1080".into(),
            refresh: "60".into(),
            dpi: "96".into(),
            name: "dfy-virtual".into(),
        }
    }
}

impl DisplayDraft {
    fn spec(&self) -> Result<VirtualDisplaySpec> {
        let spec = VirtualDisplaySpec::new(
            self.width.trim().parse().context("宽度必须是整数")?,
            self.height.trim().parse().context("高度必须是整数")?,
            self.refresh.trim().parse().context("刷新率必须是整数")?,
            self.dpi.trim().parse().context("DPI 必须是整数")?,
            self.name.trim(),
        );
        spec.validate()?;
        Ok(spec)
    }
}

#[derive(Debug, Clone)]
struct Diagnostic {
    name: String,
    ok: bool,
    detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisplayAction {
    Create,
    Update,
    Destroy,
}

enum ManagerEvent {
    RelayHealth {
        id: String,
        health: HealthStatus,
    },
    Displays(Result<Vec<VirtualDisplayInfo>, String>),
    DisplayDone {
        action: DisplayAction,
        result: Result<(), String>,
    },
    ChildOutput {
        id: u64,
        line: String,
    },
}

#[derive(Debug, Clone)]
enum ChildKind {
    Viewer { device_id: Option<String> },
    Agent,
}

struct ManagedChild {
    id: u64,
    child: Child,
    kind: ChildKind,
    label: String,
    last_output: String,
}

#[derive(Clone)]
enum ConfirmAction {
    DeleteDevice(String),
    DeleteRelay(String),
    DestroyDisplay(u32),
}

struct ConfirmState {
    title: String,
    message: String,
    action: ConfirmAction,
}

enum Modal {
    None,
    Device(DeviceForm),
    Relay(RelayForm),
    Confirm(ConfirmState),
}

struct ManagerApp {
    page: Page,
    store: RelayProfileStore,
    book: DeviceBook,
    store_error: Option<String>,
    book_error: Option<String>,
    dfy_path: PathBuf,
    connect: ConnectForm,
    agent: AgentForm,
    search: String,
    display_draft: DisplayDraft,
    display_backend: Option<Arc<dyn VirtualDisplayBackend>>,
    displays: Vec<VirtualDisplayInfo>,
    display_busy: bool,
    display_error: Option<String>,
    diagnostics: Vec<Diagnostic>,
    status: String,
    status_error: bool,
    children: Vec<ManagedChild>,
    next_child_id: u64,
    relay_testing: bool,
    relay_test_remaining: usize,
    relay_test_failed: bool,
    modal: Modal,
    tx: mpsc::Sender<ManagerEvent>,
    rx: mpsc::Receiver<ManagerEvent>,
}

impl ManagerApp {
    fn new(args: Args) -> Self {
        let (store, store_error) = match RelayProfileStore::load(&args.config) {
            Ok(store) => (store, None),
            Err(err) => (
                RelayProfileStore::new(&args.config),
                Some(format!("中继配置读取失败：{err}")),
            ),
        };
        let (book, book_error) = match DeviceBook::load(&args.devices) {
            Ok(book) => (book, None),
            Err(err) => (
                DeviceBook {
                    path: args.devices.clone(),
                    devices: Vec::new(),
                },
                Some(format!("设备列表读取失败：{err}")),
            ),
        };
        let (display_backend, display_error) = match create_backend() {
            Ok(backend) => (Some(Arc::from(backend)), None),
            Err(err) => (None, Some(format!("虚拟显示后端不可用：{err}"))),
        };
        let (tx, rx) = mpsc::channel();
        let mut app = Self {
            page: args
                .tab
                .as_deref()
                .and_then(Page::parse)
                .unwrap_or(Page::Devices),
            store,
            book,
            store_error,
            book_error,
            dfy_path: resolve_dfy_path(args.dfy),
            connect: ConnectForm::with_addr("127.0.0.1:21116"),
            agent: AgentForm::default(),
            search: String::new(),
            display_draft: DisplayDraft::default(),
            display_backend,
            displays: Vec::new(),
            display_busy: false,
            display_error,
            diagnostics: Vec::new(),
            status: "就绪。连接一台设备，或允许这台电脑被远程控制。".into(),
            status_error: false,
            children: Vec::new(),
            next_child_id: 1,
            relay_testing: false,
            relay_test_remaining: 0,
            relay_test_failed: false,
            modal: Modal::None,
            tx,
            rx,
        };
        app.refresh_diagnostics();
        app.refresh_displays();
        app
    }

    fn set_status(&mut self, message: impl Into<String>, error: bool) {
        self.status = message.into();
        self.status_error = error;
    }

    fn drain_events(&mut self) {
        while let Ok(event) = self.rx.try_recv() {
            match event {
                ManagerEvent::RelayHealth { id, health } => {
                    self.relay_test_failed |= !health.ok;
                    if let Err(err) = self.store.update_health(&id, health.clone()) {
                        self.set_status(format!("保存中继健康状态失败：{err}"), true);
                    } else if let Err(err) = self.store.save() {
                        self.set_status(format!("保存中继配置失败：{err}"), true);
                    }
                    self.relay_test_remaining = self.relay_test_remaining.saturating_sub(1);
                    if self.relay_test_remaining == 0 {
                        self.relay_testing = false;
                        self.set_status(
                            if self.relay_test_failed {
                                "中继测试完成，部分服务器不可达"
                            } else {
                                "中继测试完成，全部可达"
                            },
                            self.relay_test_failed,
                        );
                    }
                }
                ManagerEvent::Displays(result) => {
                    match result {
                        Ok(displays) => {
                            self.displays = displays;
                            self.display_error = None;
                        }
                        Err(err) => {
                            self.display_error = Some(err);
                            self.displays.clear();
                        }
                    }
                    self.display_busy = false;
                }
                ManagerEvent::DisplayDone { action, result } => {
                    self.display_busy = false;
                    match result {
                        Ok(()) => {
                            let message = match action {
                                DisplayAction::Create => "虚拟显示已创建",
                                DisplayAction::Update => "虚拟显示参数已更新",
                                DisplayAction::Destroy => "虚拟显示已删除",
                            };
                            self.set_status(message, false);
                            self.refresh_displays();
                        }
                        Err(err) => self.set_status(err, true),
                    }
                }
                ManagerEvent::ChildOutput { id, line } => {
                    if let Some(child) = self.children.iter_mut().find(|child| child.id == id) {
                        child.last_output = line;
                    }
                }
            }
        }
    }

    fn poll_children(&mut self) {
        let mut exited = Vec::new();
        for child in &mut self.children {
            if let Ok(Some(status)) = child.child.try_wait() {
                exited.push((
                    child.id,
                    child.kind.clone(),
                    child.label.clone(),
                    child.last_output.clone(),
                    status.code(),
                ));
            }
        }
        self.children
            .retain(|child| !exited.iter().any(|(id, ..)| *id == child.id));
        for (_, kind, label, output, code) in exited {
            if let ChildKind::Viewer {
                device_id: Some(id),
            } = &kind
            {
                if let Err(err) = self.book.touch(id).and_then(|_| self.book.save()) {
                    self.set_status(format!("更新最近使用时间失败：{err}"), true);
                }
            }
            let exit = code.map_or_else(|| "信号终止".to_string(), |code| format!("退出码 {code}"));
            let detail = if output.is_empty() {
                format!("{label} 已{exit}")
            } else {
                format!("{label} 已{exit}：{output}")
            };
            self.set_status(detail, code != Some(0));
        }
    }

    fn refresh_diagnostics(&mut self) {
        let dfy_exists = self.dfy_path.exists();
        let config_ready = self
            .store
            .path()
            .parent()
            .map(|parent| parent.exists() || parent.as_os_str().is_empty())
            .unwrap_or(true);
        let selected = self.store.selected();
        let relay_ok = selected.is_some_and(|profile| profile.health.ok);
        let backend_ok = self
            .display_backend
            .as_ref()
            .is_some_and(|backend| backend.is_available());
        self.diagnostics = vec![
            Diagnostic {
                name: "dfy 可执行文件".into(),
                ok: dfy_exists,
                detail: self.dfy_path.display().to_string(),
            },
            Diagnostic {
                name: "配置目录".into(),
                ok: config_ready,
                detail: self
                    .store
                    .path()
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .display()
                    .to_string(),
            },
            Diagnostic {
                name: "当前中继".into(),
                ok: relay_ok,
                detail: selected.map_or_else(
                    || "尚未配置中继；直连不受影响".into(),
                    |profile| {
                        profile.health.error.clone().unwrap_or_else(|| {
                            profile.health.rtt_ms.map_or_else(
                                || "已配置，尚未测试".into(),
                                |rtt| format!("{} · {rtt:.1} ms", profile.endpoint()),
                            )
                        })
                    },
                ),
            },
            Diagnostic {
                name: "虚拟显示后端".into(),
                ok: backend_ok,
                detail: if backend_ok {
                    "驱动或后端可用".into()
                } else {
                    "未安装或当前环境不支持；物理桌面仍可使用".into()
                },
            },
            Diagnostic {
                name: "活动子进程".into(),
                ok: true,
                detail: format!("{} 个 Viewer / Agent 正在运行", self.children.len()),
            },
        ];
    }

    fn test_relay(&mut self, id: String) {
        if self.relay_testing {
            self.set_status("已有中继测试正在运行", true);
            return;
        }
        let Some(profile) = self.store.get(&id).cloned() else {
            self.set_status("找不到要测试的中继档案", true);
            return;
        };
        self.relay_testing = true;
        self.relay_test_remaining = 1;
        self.relay_test_failed = false;
        self.set_status(format!("正在测试 {}…", profile.name), false);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let health = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|err| err.to_string())
                .and_then(|runtime| {
                    runtime
                        .block_on(probe_relay_health(&profile, Duration::from_secs(5)))
                        .map_err(|err| format!("{err:#}"))
                })
                .unwrap_or_else(HealthStatus::failure);
            let _ = tx.send(ManagerEvent::RelayHealth {
                id: profile.id,
                health,
            });
        });
    }

    fn test_all_relays(&mut self) {
        if self.relay_testing {
            return;
        }
        let profiles = self.store.profiles().to_vec();
        if profiles.is_empty() {
            self.set_status("先添加一台中继服务器", true);
            return;
        }
        self.relay_testing = true;
        self.relay_test_remaining = profiles.len();
        self.relay_test_failed = false;
        self.set_status(format!("正在测试 {} 台中继…", profiles.len()), false);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            for profile in profiles {
                let health = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|err| err.to_string())
                    .and_then(|runtime| {
                        runtime
                            .block_on(probe_relay_health(&profile, Duration::from_secs(5)))
                            .map_err(|err| format!("{err:#}"))
                    })
                    .unwrap_or_else(HealthStatus::failure);
                let _ = tx.send(ManagerEvent::RelayHealth {
                    id: profile.id,
                    health,
                });
            }
        });
    }

    fn refresh_displays(&mut self) {
        if self.display_busy {
            return;
        }
        let Some(backend) = self.display_backend.clone() else {
            self.display_error = Some("当前平台没有可用的虚拟显示后端".into());
            return;
        };
        self.display_busy = true;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = backend.list().map_err(|err| err.to_string());
            let _ = tx.send(ManagerEvent::Displays(result));
        });
    }

    fn create_display(&mut self) {
        let spec = match self.display_draft.spec() {
            Ok(spec) => spec,
            Err(err) => {
                self.set_status(format!("显示参数无效：{err}"), true);
                return;
            }
        };
        self.run_display_operation(DisplayAction::Create, spec, None);
    }

    fn update_display(&mut self, id: u32) {
        let spec = match self.display_draft.spec() {
            Ok(spec) => spec,
            Err(err) => {
                self.set_status(format!("显示参数无效：{err}"), true);
                return;
            }
        };
        self.run_display_operation(DisplayAction::Update, spec, Some(id));
    }

    fn destroy_display(&mut self, id: u32) {
        self.run_display_operation(
            DisplayAction::Destroy,
            VirtualDisplaySpec::default(),
            Some(id),
        );
    }

    fn run_display_operation(
        &mut self,
        action: DisplayAction,
        spec: VirtualDisplaySpec,
        id: Option<u32>,
    ) {
        if self.display_busy {
            self.set_status("另一项显示操作正在执行", true);
            return;
        }
        let Some(backend) = self.display_backend.clone() else {
            self.set_status("虚拟显示后端不可用", true);
            return;
        };
        self.display_busy = true;
        self.set_status("正在操作虚拟显示…", false);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = match (action, id) {
                (DisplayAction::Create, None) => backend.create(&spec).map(|_| ()),
                (DisplayAction::Update, Some(id)) => backend.update(id, &spec),
                (DisplayAction::Destroy, Some(id)) => backend.destroy(id),
                _ => Err(anyhow::anyhow!("虚拟显示操作参数不完整")),
            }
            .map_err(|err| err.to_string());
            let _ = tx.send(ManagerEvent::DisplayDone { action, result });
        });
    }

    fn save_relay_form(&mut self, form: &RelayForm) -> Result<()> {
        let name = form.name.trim().to_string();
        let host = form.host.trim().to_string();
        if name.is_empty() || host.is_empty() {
            bail!("请填写中继名称和主机地址");
        }
        let port = form.port.trim().parse().context("端口必须是整数")?;
        let priority = form.priority.trim().parse().context("优先级必须是整数")?;
        let key = if form.key.get().is_empty() {
            form.id
                .as_ref()
                .and_then(|id| self.store.get(id))
                .map(|profile| profile.key_config().to_string())
                .context("新增中继必须填写密钥")?
        } else {
            form.key.get().to_string()
        };
        let profile_id = if let Some(id) = &form.id {
            id.clone()
        } else if form.profile_id.trim().is_empty() {
            format!("relay-{}-{}", now_ms(), random_suffix(4))
        } else {
            form.profile_id.trim().to_string()
        };
        let region = form.region.trim().to_string();
        let mut profile = RelayProfile::new(
            profile_id,
            name,
            host,
            port,
            key,
            (!region.is_empty()).then_some(region),
            priority,
            form.preferred,
            form.tls,
        )?;
        if let Some(id) = &form.id {
            if let Some(previous) = self.store.get(id) {
                profile.health = previous.health.clone();
            }
            if self.store.get_mut(id).is_none() {
                bail!("要编辑的中继档案不存在");
            }
            let current = self.store.get_mut(id).expect("checked above");
            *current = profile;
        } else {
            self.store.add(profile)?;
        }
        self.store.save()?;
        self.set_status("中继档案已保存", false);
        Ok(())
    }

    fn save_device_form(&mut self, form: &DeviceForm) -> Result<()> {
        let name = form.name.trim().to_string();
        let addr = form.addr.trim().to_string();
        if name.is_empty() || addr.is_empty() {
            bail!("请填写设备名称和地址");
        }
        validate_addr(&addr)?;
        let entry = if let Some(id) = &form.id {
            let mut entry = self.book.get(id).cloned().context("要编辑的设备不存在")?;
            entry.name = name;
            entry.addr = addr;
            entry.favorite = form.favorite;
            entry.use_relay = form.use_relay;
            entry.relay_tag = form.relay_tag.trim().into();
            entry.fingerprint = form.fingerprint.trim().into();
            entry
        } else {
            let mut entry = DeviceEntry::new(name, addr);
            entry.favorite = form.favorite;
            entry.use_relay = form.use_relay;
            entry.relay_tag = form.relay_tag.trim().into();
            entry.fingerprint = form.fingerprint.trim().into();
            entry
        };
        if entry.use_relay {
            validate_relay_tag(&entry.relay_tag)?;
        }
        if !entry.fingerprint.is_empty() {
            validate_fingerprint(&entry.fingerprint)?;
        }
        self.book.upsert(entry);
        self.book.save()?;
        self.set_status("设备已保存；连接口令不会写入文件", false);
        Ok(())
    }

    fn connect_form(&mut self) {
        let password = Zeroizing::new(self.connect.password.get().to_string());
        let request = ViewerRequest {
            label: if self.connect.name.trim().is_empty() {
                self.connect.addr.trim().to_string()
            } else {
                self.connect.name.trim().to_string()
            },
            addr: self.connect.addr.trim().to_string(),
            use_relay: self.connect.use_relay,
            relay_tag: self.connect.relay_tag.trim().to_string(),
            fingerprint: self.connect.fingerprint.trim().to_string(),
            clipboard: self.connect.clipboard,
            device_id: None,
        };
        match self.launch_viewer(request, &password) {
            Ok(()) => self.connect.password.clear(),
            Err(err) => self.set_status(err.to_string(), true),
        }
    }

    fn connect_device(&mut self, id: String) {
        let Some(entry) = self.book.get(&id).cloned() else {
            self.set_status("设备已不存在，请刷新列表", true);
            return;
        };
        let password = Zeroizing::new(self.connect.password.get().to_string());
        match self.launch_viewer(
            ViewerRequest {
                label: entry.name,
                addr: entry.addr,
                use_relay: entry.use_relay,
                relay_tag: entry.relay_tag,
                fingerprint: entry.fingerprint,
                clipboard: self.connect.clipboard,
                device_id: Some(id),
            },
            &password,
        ) {
            Ok(()) => self.connect.password.clear(),
            Err(err) => self.set_status(err.to_string(), true),
        }
    }

    fn launch_viewer(&mut self, request: ViewerRequest, password: &str) -> Result<()> {
        validate_password(password)?;
        validate_addr(&request.addr)?;
        if request.use_relay {
            validate_relay_tag(&request.relay_tag)?;
            if self.store.selected().is_none() {
                bail!("请先添加并选择一台中继服务器");
            }
        }
        if !request.fingerprint.is_empty() {
            validate_fingerprint(&request.fingerprint)?;
        }
        let relay_id = self.store.selected().map(|profile| profile.id.clone());
        let command = view_command(
            &self.dfy_path,
            &request,
            self.store.path(),
            relay_id.as_deref(),
        )?;
        self.spawn_dfy(
            command,
            password,
            ChildKind::Viewer {
                device_id: request.device_id,
            },
            "远控窗口",
        )?;
        self.set_status(format!("正在连接 {}…", request.label), false);
        Ok(())
    }

    fn start_agent(&mut self) -> Result<()> {
        if self.agent_running() {
            bail!("被控端已经在本机运行");
        }
        validate_password(self.agent.password.get())?;
        let host = self.agent.host.trim().to_string();
        let port = self.agent.port.trim().parse().context("端口必须是整数")?;
        let bind = format!("{host}:{port}")
            .parse::<std::net::SocketAddr>()
            .context("监听地址必须是 IP:端口，例如 0.0.0.0:21116")?;
        if bind.port() == 0 {
            bail!("监听端口不能为 0");
        }
        if self.agent.use_relay {
            validate_relay_tag(&self.agent.relay_tag)?;
            if self.store.selected().is_none() {
                bail!("请先添加并选择一台中继服务器");
            }
        }
        let request = AgentRequest {
            host,
            port,
            allow_input: self.agent.allow_input,
            allow_clipboard: self.agent.allow_clipboard,
            audio_out: self.agent.audio_out,
            use_relay: self.agent.use_relay,
            relay_tag: self.agent.relay_tag.trim().to_string(),
            virtual_displays: self.agent.virtual_displays,
            virtual_monitor: self.agent.virtual_monitor,
        };
        let relay_id = self.store.selected().map(|profile| profile.id.clone());
        let command = agent_command(
            &self.dfy_path,
            &request,
            self.store.path(),
            relay_id.as_deref(),
        )?;
        let password = Zeroizing::new(self.agent.password.get().to_string());
        self.spawn_dfy(command, &password, ChildKind::Agent, "被控端")?;
        self.set_status(format!("被控端已启动：{}:{port}", request.host), false);
        Ok(())
    }

    fn spawn_dfy(
        &mut self,
        mut command: Command,
        password: &str,
        kind: ChildKind,
        label: &str,
    ) -> Result<()> {
        validate_password(password)?;
        configure_command(&mut command);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .with_context(|| format!("启动 {} 失败", self.dfy_path.display()))?;
        let id = self.next_child_id;
        self.next_child_id = self.next_child_id.saturating_add(1);
        let input = Zeroizing::new(format!("{password}\n"));
        let write_result = child
            .stdin
            .take()
            .context("无法打开口令输入管道")?
            .write_all(input.as_bytes());
        if let Err(err) = write_result {
            let _ = child.kill();
            let _ = child.wait();
            bail!("写入会话口令失败：{err}");
        }
        if let Some(stderr) = child.stderr.take() {
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                let reader = io::BufReader::new(stderr);
                for line in reader.lines().map_while(Result::ok) {
                    if tx.send(ManagerEvent::ChildOutput { id, line }).is_err() {
                        break;
                    }
                }
            });
        }
        self.children.push(ManagedChild {
            id,
            child,
            kind,
            label: label.to_string(),
            last_output: String::new(),
        });
        Ok(())
    }

    fn agent_running(&self) -> bool {
        self.children
            .iter()
            .any(|child| matches!(child.kind, ChildKind::Agent))
    }

    fn viewer_count(&self) -> usize {
        self.children
            .iter()
            .filter(|child| matches!(child.kind, ChildKind::Viewer { .. }))
            .count()
    }

    fn stop_child(&mut self, id: u64) {
        if let Some(index) = self.children.iter().position(|child| child.id == id) {
            let mut child = self.children.remove(index);
            let label = child.label.clone();
            let _ = child.child.kill();
            let _ = child.child.wait();
            self.set_status(format!("{label} 已停止"), false);
        }
    }

    fn stop_agent(&mut self) {
        if let Some(id) = self
            .children
            .iter()
            .find(|child| matches!(child.kind, ChildKind::Agent))
            .map(|child| child.id)
        {
            self.stop_child(id);
        }
    }

    fn stop_all(&mut self) {
        let ids: Vec<u64> = self.children.iter().map(|child| child.id).collect();
        for id in ids {
            self.stop_child(id);
        }
        self.agent.password.clear();
        self.connect.password.clear();
    }

    fn request_confirmation(&mut self, title: &str, message: &str, action: ConfirmAction) {
        self.modal = Modal::Confirm(ConfirmState {
            title: title.into(),
            message: message.into(),
            action,
        });
    }

    fn confirm_action(&mut self, action: ConfirmAction) -> Result<()> {
        match action {
            ConfirmAction::DeleteDevice(id) => {
                self.book.remove(&id)?;
                self.book.save()?;
                self.set_status("设备已删除", false);
            }
            ConfirmAction::DeleteRelay(id) => {
                self.store.remove(&id)?;
                self.store.save()?;
                self.set_status("中继档案已删除", false);
            }
            ConfirmAction::DestroyDisplay(id) => self.destroy_display(id),
        }
        Ok(())
    }
}

impl Drop for ManagerApp {
    fn drop(&mut self) {
        self.stop_all();
    }
}

struct AgentRequest {
    host: String,
    port: u16,
    allow_input: bool,
    allow_clipboard: bool,
    audio_out: bool,
    use_relay: bool,
    relay_tag: String,
    virtual_displays: u32,
    virtual_monitor: u32,
}

fn view_command(
    dfy: &Path,
    request: &ViewerRequest,
    relay_config: &Path,
    relay_id: Option<&str>,
) -> Result<Command> {
    validate_addr(&request.addr)?;
    if !request.fingerprint.is_empty() {
        validate_fingerprint(&request.fingerprint)?;
    }
    if request.use_relay {
        validate_relay_tag(&request.relay_tag)?;
        if relay_id.is_none() {
            bail!("没有可用的中继档案");
        }
    }
    let mut command = Command::new(dfy);
    command.args(["view", "--ui", "--control", "--password-stdin"]);
    command.arg("--addr").arg(&request.addr);
    if request.clipboard {
        command.arg("--clipboard");
    }
    if !request.fingerprint.is_empty() {
        command
            .arg("--expect-fingerprint")
            .arg(&request.fingerprint);
    }
    if request.use_relay {
        command
            .arg("--relay-profile")
            .arg(relay_id.expect("checked above"));
        command.arg("--relay-profiles-config").arg(relay_config);
        command.arg("--relay-tag").arg(&request.relay_tag);
    }
    Ok(command)
}

fn agent_command(
    dfy: &Path,
    request: &AgentRequest,
    relay_config: &Path,
    relay_id: Option<&str>,
) -> Result<Command> {
    if request.use_relay {
        validate_relay_tag(&request.relay_tag)?;
        if relay_id.is_none() {
            bail!("没有可用的中继档案");
        }
    }
    let mut command = Command::new(dfy);
    command.args(["agent", "--password-stdin"]);
    command.arg("--host").arg(&request.host);
    command.arg("--port").arg(request.port.to_string());
    if request.allow_input {
        command.arg("--allow-input");
    }
    if request.allow_clipboard {
        command.arg("--allow-clipboard");
    }
    if request.audio_out {
        command.arg("--audio-out");
    }
    if request.virtual_displays > 0 {
        command
            .arg("--virtual-displays")
            .arg(request.virtual_displays.to_string());
        command
            .arg("--virtual-monitor")
            .arg(request.virtual_monitor.to_string());
    }
    if request.use_relay {
        command
            .arg("--relay-profile")
            .arg(relay_id.expect("checked above"));
        command.arg("--relay-profiles-config").arg(relay_config);
        command.arg("--relay-tag").arg(&request.relay_tag);
    }
    Ok(command)
}

fn configure_command(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = command;
}

fn validate_password(password: &str) -> Result<()> {
    if !(MIN_PASSWORD_BYTES..=MAX_PASSWORD_BYTES).contains(&password.len())
        || password.trim().is_empty()
    {
        bail!("会话口令必须为 16–1024 字节，且不能全为空白");
    }
    Ok(())
}

fn validate_addr(addr: &str) -> Result<()> {
    addr.parse::<std::net::SocketAddr>()
        .with_context(|| format!("地址必须是 IP:端口，例如 192.168.1.20:21116；收到 {addr}"))?;
    Ok(())
}

fn validate_relay_tag(tag: &str) -> Result<()> {
    if tag.len() != 32 || !tag.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("中继会合 tag 必须是 32 位十六进制字符");
    }
    Ok(())
}

fn validate_fingerprint(fingerprint: &str) -> Result<()> {
    if fingerprint.len() != 64 || !fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("证书指纹必须是 64 位十六进制字符");
    }
    Ok(())
}

async fn resolve_relay_addr(profile: &RelayProfile) -> Result<SocketAddr> {
    if let Ok(addr) = profile.addr() {
        return Ok(addr);
    }
    let lookup = format!("{}:{}", profile.host, profile.port);
    tokio::net::lookup_host(lookup)
        .await?
        .next()
        .with_context(|| format!("中继主机 {} 没有可用地址", profile.host))
}

async fn probe_relay_health(profile: &RelayProfile, timeout: Duration) -> Result<HealthStatus> {
    const PROBE_BYTES: usize = 1200;
    const PROBE_PACKETS: u8 = 8;
    const PROBE_MAGIC: [u8; 4] = [0xDF, 0x59, 0x50, 0x52];

    let addr = resolve_relay_addr(profile).await?;
    let key = profile.key()?;
    let mut tag = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut tag);
    let options = RelaySocketOptions {
        bind_timeout: timeout.max(Duration::from_millis(1)),
        retry_interval: Duration::from_millis(200),
        keepalive_interval: Duration::from_secs(3600),
    };
    let config = RelayClientConfig {
        relay_addr: addr,
        tag,
        role: Role::Client,
        key: *key.as_bytes(),
        nonce: dfy_transport::now_us(),
    };
    let started = Instant::now();
    let client = bind_relay_socket_with(&config, &options)
        .await
        .context("配对中继失败")?;
    let rtt = started.elapsed().as_secs_f64() * 1000.0;
    let agent = bind_relay_socket_with(
        &RelayClientConfig {
            role: Role::Agent,
            nonce: dfy_transport::now_us().wrapping_add(1),
            ..config
        },
        &options,
    )
    .await
    .context("配对中继对端失败")?;
    let relay_addr = client.peer_addr();
    let (lost, bps) = tokio::task::spawn_blocking(move || -> Result<(u64, u64)> {
        let mut payload = vec![0u8; PROBE_BYTES];
        payload[..4].copy_from_slice(&PROBE_MAGIC);
        let send_started = Instant::now();
        for index in 0..PROBE_PACKETS {
            payload[4] = index;
            client
                .socket()
                .send_to(&payload, relay_addr)
                .context("发送中继探测包失败")?;
            std::thread::sleep(Duration::from_millis(2));
        }
        let elapsed = send_started.elapsed().max(Duration::from_millis(1));
        let mut received = 0u64;
        let mut buffer = [0u8; PROBE_BYTES + 64];
        let deadline = Instant::now() + timeout.max(Duration::from_millis(500));
        while Instant::now() < deadline && received < u64::from(PROBE_PACKETS) {
            match agent.socket().recv(&mut buffer) {
                Ok(size) if size >= 4 && buffer[..4] == PROBE_MAGIC => received += 1,
                Ok(_) => {}
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(err) => return Err(err.into()),
            }
        }
        let bytes = received * PROBE_BYTES as u64;
        let bps = bytes
            .saturating_mul(8)
            .checked_div(elapsed.as_millis().max(1) as u64)
            .unwrap_or(0)
            .saturating_mul(1000);
        Ok((u64::from(PROBE_PACKETS) - received, bps))
    })
    .await
    .context("中继探测任务失败")??;
    let mut health = HealthStatus::success(rtt);
    health.loss_ppm = Some(
        lost.saturating_mul(1_000_000)
            .checked_div(u64::from(PROBE_PACKETS))
            .unwrap_or(1_000_000) as u32,
    );
    health.bandwidth_bps = Some(bps);
    Ok(health)
}

fn random_suffix(length: usize) -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(length)
        .map(char::from)
        .collect()
}

fn generate_password() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";
    let mut rng = rand::thread_rng();
    (0..24)
        .map(|_| char::from(ALPHABET[rng.gen_range(0..ALPHABET.len())]))
        .collect()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn resolve_dfy_path(explicit: Option<PathBuf>) -> PathBuf {
    if let Some(path) = explicit {
        return path;
    }
    #[cfg(windows)]
    let name = "dfy.exe";
    #[cfg(not(windows))]
    let name = "dfy";
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let sibling = parent.join(name);
            if sibling.exists() {
                return sibling;
            }
        }
    }
    PathBuf::from(name)
}

impl eframe::App for ManagerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();
        self.poll_children();
        ctx.request_repaint_after(Duration::from_millis(120));

        self.draw_header(ctx);
        self.draw_navigation(ctx);
        CentralPanel::default().show(ctx, |ui| {
            ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| match self.page {
                    Page::Devices => self.ui_devices(ui),
                    Page::Agent => self.ui_agent(ui),
                    Page::Relays => self.ui_relays(ui),
                    Page::Displays => self.ui_displays(ui),
                    Page::Diagnostics => self.ui_diagnostics(ui),
                });
        });
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let color = if self.status_error {
                    Color32::from_rgb(255, 138, 128)
                } else {
                    Color32::from_rgb(156, 211, 178)
                };
                ui.label(RichText::new("●").color(color));
                ui.label(&self.status);
            });
        });
        self.draw_modal(ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.stop_all();
    }
}

impl ManagerApp {
    fn draw_header(&self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.add_space(8.0);
                ui.label(RichText::new(APP_TITLE).size(22.0).strong());
                ui.add_space(12.0);
                ui.label(
                    RichText::new("原生远程桌面")
                        .size(13.0)
                        .color(Color32::from_rgb(150, 164, 184)),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(format!("{} 个远控窗口", self.viewer_count()));
                    if self.agent_running() {
                        ui.label(
                            RichText::new("● 本机可被控制").color(Color32::from_rgb(116, 211, 152)),
                        );
                    }
                });
            });
            ui.add_space(10.0);
        });
    }

    fn draw_navigation(&mut self, ctx: &egui::Context) {
        let compact = ctx.screen_rect().width() < 900.0;
        let width = if compact { 62.0 } else { 184.0 };
        egui::SidePanel::left("navigation")
            .exact_width(width)
            .show(ctx, |ui| {
                ui.add_space(12.0);
                for page in [
                    Page::Devices,
                    Page::Agent,
                    Page::Relays,
                    Page::Displays,
                    Page::Diagnostics,
                ] {
                    let text = if compact {
                        page.icon().to_string()
                    } else {
                        format!("{}  {}", page.icon(), page.label())
                    };
                    let response = ui.add_sized(
                        [width - 16.0, 42.0],
                        egui::SelectableLabel::new(self.page == page, text),
                    );
                    if response.clicked() {
                        self.page = page;
                    }
                    if !compact {
                        response.on_hover_text(page.label());
                    }
                    ui.add_space(4.0);
                }
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
                    ui.add_space(8.0);
                    if compact {
                        ui.label("v0.1");
                    } else {
                        ui.label(
                            RichText::new("v0.1 · PolyForm NC")
                                .small()
                                .color(Color32::from_rgb(126, 139, 158)),
                        );
                    }
                });
            });
    }

    fn ui_devices(&mut self, ui: &mut egui::Ui) {
        ui.heading("连接设备");
        ui.label("输入对方提供的地址和会话口令。密码只用于本次连接，不会保存。");
        ui.add_space(10.0);
        card(ui, |ui| {
            ui.label(RichText::new("快速连接").strong());
            ui.add_space(8.0);
            form_grid(ui, "quick-connect", |ui| {
                grid_label(ui, "名称（可选）", "connect-name", |ui, width| {
                    ui.add_sized(
                        [width, 0.0],
                        egui::TextEdit::singleline(&mut self.connect.name),
                    );
                });
                grid_label(ui, "设备地址", "connect-addr", |ui, width| {
                    ui.add_sized(
                        [width, 0.0],
                        egui::TextEdit::singleline(&mut self.connect.addr)
                            .hint_text("192.168.1.20:21116"),
                    );
                });
                grid_label(ui, "会话口令", "connect-password", |ui, width| {
                    ui.add_sized(
                        [width, 0.0],
                        egui::TextEdit::singleline(self.connect.password.get_mut())
                            .password(!self.connect.show_password)
                            .hint_text("至少 16 字节"),
                    );
                });
                ui.label("显示口令");
                ui.checkbox(&mut self.connect.show_password, "");
                ui.end_row();
                grid_label(ui, "中继会合 tag", "connect-tag", |ui, width| {
                    ui.add_enabled(
                        self.connect.use_relay,
                        egui::TextEdit::singleline(&mut self.connect.relay_tag)
                            .desired_width(width)
                            .hint_text("使用中继时填写 32 位 hex"),
                    );
                });
                ui.label("使用当前中继");
                ui.checkbox(&mut self.connect.use_relay, "");
                ui.end_row();
                grid_label(
                    ui,
                    "证书指纹（可选）",
                    "connect-fingerprint",
                    |ui, width| {
                        ui.add_sized(
                            [width, 0.0],
                            egui::TextEdit::singleline(&mut self.connect.fingerprint)
                                .hint_text("64 位 hex，用于固定校验对端"),
                        );
                    },
                );
                ui.label("剪贴板同步");
                ui.checkbox(&mut self.connect.clipboard, "需对端授权");
                ui.end_row();
            });
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ui
                    .add_sized([132.0, 36.0], egui::Button::new("连接").fill(accent()))
                    .clicked()
                {
                    self.connect_form();
                }
                if ui.button("保存为新设备").clicked() {
                    self.modal = Modal::Device(DeviceForm {
                        name: self.connect.name.clone(),
                        addr: self.connect.addr.clone(),
                        use_relay: self.connect.use_relay,
                        relay_tag: self.connect.relay_tag.clone(),
                        fingerprint: self.connect.fingerprint.clone(),
                        ..DeviceForm::default()
                    });
                }
                if ui.button("清空口令").clicked() {
                    self.connect.password.clear();
                }
            });
            if self.connect.use_relay {
                ui.add_space(6.0);
                let current = self
                    .store
                    .current_display()
                    .unwrap_or_else(|| "尚未配置中继".into());
                ui.label(RichText::new(format!("当前中继：{current}")).small());
            }
        });

        ui.add_space(18.0);
        ui.horizontal(|ui| {
            ui.heading("我的设备");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("＋ 添加设备").clicked() {
                    self.modal = Modal::Device(DeviceForm::default());
                }
                ui.add_sized(
                    [220.0, 28.0],
                    egui::TextEdit::singleline(&mut self.search).hint_text("搜索名称或地址"),
                );
            });
        });
        if let Some(err) = &self.book_error {
            ui.colored_label(Color32::from_rgb(255, 160, 120), err);
        }
        let search = self.search.to_lowercase();
        let entries: Vec<DeviceEntry> = self
            .book
            .devices
            .iter()
            .filter(|entry| {
                search.is_empty()
                    || entry.name.to_lowercase().contains(&search)
                    || entry.addr.to_lowercase().contains(&search)
            })
            .cloned()
            .collect();
        if entries.is_empty() {
            empty_state(
                ui,
                "还没有匹配的设备",
                "添加设备后会固定显示在这里，连接时只需输入口令。",
            );
        }
        for entry in entries {
            self.device_row(ui, &entry);
        }
    }

    fn device_row(&mut self, ui: &mut egui::Ui, entry: &DeviceEntry) {
        card(ui, |ui| {
            let compact = ui.available_width() < 620.0;
            if compact {
                device_summary(ui, entry);
                ui.horizontal(|ui| {
                    if ui.button("连接").clicked() {
                        self.connect_device(entry.id.clone());
                    }
                    if ui.button("编辑").clicked() {
                        self.modal = Modal::Device(DeviceForm::from_entry(entry));
                    }
                    if ui.button("删除").clicked() {
                        self.request_confirmation(
                            "删除设备",
                            &format!("确定删除“{}”吗？", entry.name),
                            ConfirmAction::DeleteDevice(entry.id.clone()),
                        );
                    }
                });
            } else {
                ui.horizontal_top(|ui| {
                    device_summary(ui, entry);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("删除").clicked() {
                            self.request_confirmation(
                                "删除设备",
                                &format!("确定删除“{}”吗？", entry.name),
                                ConfirmAction::DeleteDevice(entry.id.clone()),
                            );
                        }
                        if ui.button("编辑").clicked() {
                            self.modal = Modal::Device(DeviceForm::from_entry(entry));
                        }
                        let favorite = if entry.favorite {
                            "★ 收藏"
                        } else {
                            "☆ 收藏"
                        };
                        if ui.button(favorite).clicked() {
                            if let Some(current) = self.book.get_mut(&entry.id) {
                                current.favorite = !current.favorite;
                                if let Err(err) = self.book.save() {
                                    self.set_status(format!("保存设备失败：{err}"), true);
                                }
                            }
                            self.book.sort();
                        }
                        if ui.add(egui::Button::new("连接").fill(accent())).clicked() {
                            self.connect_device(entry.id.clone());
                        }
                    });
                });
            }
        });
        ui.add_space(8.0);
    }

    fn ui_agent(&mut self, ui: &mut egui::Ui) {
        ui.heading("允许控制这台电脑");
        ui.label("启动被控端后，把地址和口令提供给已获得授权的控制端。默认只允许键鼠输入。");
        ui.add_space(10.0);
        card(ui, |ui| {
            ui.horizontal(|ui| {
                if self.agent_running() {
                    ui.label(
                        RichText::new("● 被控端正在运行")
                            .strong()
                            .color(Color32::from_rgb(116, 211, 152)),
                    );
                } else {
                    ui.label(
                        RichText::new("○ 被控端未启动").color(Color32::from_rgb(160, 170, 186)),
                    );
                }
            });
            ui.add_space(10.0);
            form_grid(ui, "agent-form", |ui| {
                grid_label(ui, "监听地址", "agent-host", |ui, width| {
                    ui.add_enabled(
                        !self.agent_running(),
                        egui::TextEdit::singleline(&mut self.agent.host).desired_width(width),
                    );
                });
                grid_label(ui, "端口", "agent-port", |ui, width| {
                    ui.add_enabled(
                        !self.agent_running(),
                        egui::TextEdit::singleline(&mut self.agent.port).desired_width(width),
                    );
                });
                grid_label(ui, "会话口令", "agent-password", |ui, width| {
                    ui.horizontal(|ui| {
                        ui.add_enabled(
                            !self.agent_running(),
                            egui::TextEdit::singleline(self.agent.password.get_mut())
                                .password(!self.agent.show_password)
                                .desired_width(width - 82.0),
                        );
                        if ui.button("生成").clicked() {
                            self.agent.password = Secret::new(generate_password());
                        }
                    });
                });
                ui.label("显示口令");
                ui.checkbox(&mut self.agent.show_password, "仅在本窗口显示");
                ui.end_row();
                grid_label(ui, "中继会合 tag", "agent-tag", |ui, width| {
                    ui.add_enabled(
                        self.agent.use_relay,
                        egui::TextEdit::singleline(&mut self.agent.relay_tag).desired_width(width),
                    );
                });
                ui.label("使用当前中继");
                ui.checkbox(&mut self.agent.use_relay, "同时提供直连地址");
                ui.end_row();
                ui.label("虚拟显示数量");
                ui.add(egui::DragValue::new(&mut self.agent.virtual_displays).clamp_range(0..=16));
                ui.end_row();
                ui.label("采集虚拟显示索引");
                ui.add_enabled(
                    self.agent.virtual_displays > 0,
                    egui::DragValue::new(&mut self.agent.virtual_monitor)
                        .clamp_range(0..=self.agent.virtual_displays.saturating_sub(1)),
                );
                ui.end_row();
            });
            ui.add_space(10.0);
            ui.label(RichText::new("本次会话权限").strong());
            ui.checkbox(&mut self.agent.allow_input, "允许远程键鼠输入");
            ui.checkbox(&mut self.agent.allow_clipboard, "允许双向剪贴板同步");
            ui.checkbox(&mut self.agent.audio_out, "发送本机音频");
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if self.agent_running() {
                    if ui
                        .add(egui::Button::new("停止被控端").fill(Color32::from_rgb(164, 67, 67)))
                        .clicked()
                    {
                        self.stop_agent();
                    }
                } else if ui
                    .add_sized(
                        [150.0, 36.0],
                        egui::Button::new("开始允许控制").fill(accent()),
                    )
                    .clicked()
                {
                    if let Err(err) = self.start_agent() {
                        self.set_status(err.to_string(), true);
                    }
                }
            });
            ui.add_space(6.0);
            ui.label(
                RichText::new("提示：0.0.0.0 会监听所有网卡；请在防火墙中仅允许可信网络。")
                    .small()
                    .color(Color32::from_rgb(190, 163, 112)),
            );
            if self.agent_running() {
                ui.add_space(8.0);
                ui.label(format!("连接地址：{}:{}", self.agent.host, self.agent.port));
            }
        });
    }

    fn ui_relays(&mut self, ui: &mut egui::Ui) {
        ui.heading("中继服务器");
        ui.label("使用自己的服务器。地址和密钥保存在本机配置文件中，不会上传到集中服务。");
        ui.add_space(10.0);
        if let Some(err) = &self.store_error {
            ui.colored_label(Color32::from_rgb(255, 160, 120), err);
        }
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("当前服务器").strong());
                ui.label(
                    self.store
                        .current_display()
                        .unwrap_or_else(|| "未配置".into()),
                );
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label("选择策略");
                egui::ComboBox::from_id_source("relay-strategy")
                    .selected_text(strategy_label(self.store.selection().strategy))
                    .show_ui(ui, |ui| {
                        for strategy in [
                            SelectionStrategy::Manual,
                            SelectionStrategy::PreferredFirst,
                            SelectionStrategy::LowestLatency,
                            SelectionStrategy::AutoFailover,
                        ] {
                            if ui
                                .selectable_label(
                                    self.store.selection().strategy == strategy,
                                    strategy_label(strategy),
                                )
                                .clicked()
                            {
                                self.store.set_strategy(strategy);
                                if let Err(err) = self.store.save() {
                                    self.set_status(format!("保存策略失败：{err}"), true);
                                }
                                ui.close_menu();
                            }
                        }
                    });
                if ui.button("测试全部").clicked() {
                    self.test_all_relays();
                }
                if ui.button("导入并合并").clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("中继配置", &["toml"])
                        .pick_file()
                    {
                        match RelayProfileStore::load(&path)
                            .and_then(|other| self.store.import(&other, true))
                            .and_then(|count| {
                                self.store.save()?;
                                Ok(count)
                            }) {
                            Ok(count) => self.set_status(format!("已合并 {count} 台中继"), false),
                            Err(err) => self.set_status(format!("导入失败：{err}"), true),
                        }
                    }
                }
                if ui.button("导出配置").clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .set_file_name("dfy-relay-profiles.toml")
                        .add_filter("中继配置", &["toml"])
                        .save_file()
                    {
                        match self.store.export_to_file(&path, false) {
                            Ok(()) => self.set_status("配置已导出（包含密钥，请妥善保管）", false),
                            Err(err) => self.set_status(format!("导出失败：{err}"), true),
                        }
                    }
                }
                if ui.button("导出脱敏副本").clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .set_file_name("dfy-relay-profiles.redacted.toml")
                        .add_filter("中继配置", &["toml"])
                        .save_file()
                    {
                        match self.store.export_to_file(&path, true) {
                            Ok(()) => self.set_status("脱敏副本已导出", false),
                            Err(err) => self.set_status(format!("导出失败：{err}"), true),
                        }
                    }
                }
            });
        });

        ui.add_space(16.0);
        ui.horizontal(|ui| {
            ui.heading("服务器档案");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("＋ 添加中继").clicked() {
                    self.modal = Modal::Relay(RelayForm::default());
                }
            });
        });
        if self.relay_testing {
            ui.label(RichText::new("正在测试，请稍候…").color(Color32::from_rgb(130, 180, 235)));
        }
        let profiles = self.store.profiles().to_vec();
        if profiles.is_empty() {
            empty_state(
                ui,
                "还没有中继服务器",
                "添加一台自己的中继后即可在这里测试、切换和导出。",
            );
        }
        for profile in profiles {
            self.relay_row(ui, &profile);
        }
    }

    fn relay_row(&mut self, ui: &mut egui::Ui, profile: &RelayProfile) {
        card(ui, |ui| {
            let active = self
                .store
                .selected()
                .map(|selected| selected.id == profile.id)
                .unwrap_or(false);
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&profile.name).strong());
                        if active {
                            ui.label(
                                RichText::new("当前")
                                    .small()
                                    .color(Color32::from_rgb(116, 211, 152)),
                            );
                        }
                        if profile.preferred {
                            ui.label(RichText::new("首选").small());
                        }
                    });
                    ui.label(
                        RichText::new(profile.endpoint())
                            .small()
                            .color(Color32::from_rgb(151, 166, 187)),
                    );
                    let health = if profile.health.ok {
                        profile.health.rtt_ms.map_or_else(
                            || "可达 · 延迟未知".into(),
                            |rtt| format!("可达 · {rtt:.1} ms"),
                        )
                    } else if profile.health.checked_at_ms == 0 {
                        "尚未测试".into()
                    } else {
                        profile
                            .health
                            .error
                            .clone()
                            .unwrap_or_else(|| "不可达".into())
                    };
                    ui.label(RichText::new(health).small().color(if profile.health.ok {
                        Color32::from_rgb(116, 211, 152)
                    } else {
                        Color32::from_rgb(222, 154, 112)
                    }));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("删除").clicked() {
                        self.request_confirmation(
                            "删除中继",
                            &format!("确定删除“{}”吗？", profile.name),
                            ConfirmAction::DeleteRelay(profile.id.clone()),
                        );
                    }
                    if ui.button("编辑").clicked() {
                        self.modal = Modal::Relay(RelayForm::from_profile(profile));
                    }
                    if ui.button("测试").clicked() {
                        self.test_relay(profile.id.clone());
                    }
                    if ui.button("设为首选").clicked() {
                        if let Some(current) = self.store.get_mut(&profile.id) {
                            current.preferred = !current.preferred;
                            if let Err(err) = self.store.save() {
                                self.set_status(format!("保存失败：{err}"), true);
                            }
                        }
                    }
                    if ui.button("使用").clicked() {
                        if let Err(err) = self
                            .store
                            .set_active(&profile.id)
                            .and_then(|_| self.store.save())
                        {
                            self.set_status(format!("切换失败：{err}"), true);
                        } else {
                            self.set_status(format!("已切换到 {}", profile.name), false);
                        }
                    }
                });
            });
        });
        ui.add_space(8.0);
    }

    fn ui_displays(&mut self, ui: &mut egui::Ui) {
        ui.heading("虚拟显示");
        ui.label("创建独立的远程桌面。Windows 需要已安装并验证 IddCx 驱动；Linux 使用隔离 Xvfb。");
        ui.add_space(10.0);
        card(ui, |ui| {
            let available = self
                .display_backend
                .as_ref()
                .is_some_and(|backend| backend.is_available());
            ui.label(if available {
                RichText::new("● 虚拟显示后端可用").color(Color32::from_rgb(116, 211, 152))
            } else {
                RichText::new("○ 虚拟显示后端当前不可用").color(Color32::from_rgb(222, 154, 112))
            });
            ui.add_space(8.0);
            form_grid(ui, "display-form", |ui| {
                grid_label(ui, "名称", "display-name", |ui, width| {
                    ui.add_sized(
                        [width, 0.0],
                        egui::TextEdit::singleline(&mut self.display_draft.name),
                    );
                });
                grid_label(ui, "分辨率", "display-size", |ui, width| {
                    ui.horizontal(|ui| {
                        ui.add_sized(
                            [80.0, 0.0],
                            egui::TextEdit::singleline(&mut self.display_draft.width),
                        );
                        ui.label("×");
                        ui.add_sized(
                            [80.0, 0.0],
                            egui::TextEdit::singleline(&mut self.display_draft.height),
                        );
                        let _ = width;
                    });
                });
                grid_label(ui, "刷新率", "display-refresh", |ui, width| {
                    ui.add_sized(
                        [width.min(120.0), 0.0],
                        egui::TextEdit::singleline(&mut self.display_draft.refresh),
                    );
                });
                ui.label("Hz");
                ui.end_row();
                grid_label(ui, "DPI", "display-dpi", |ui, width| {
                    ui.add_sized(
                        [width.min(120.0), 0.0],
                        egui::TextEdit::singleline(&mut self.display_draft.dpi),
                    );
                });
                ui.label("96 = 100% 缩放");
                ui.end_row();
            });
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        available && !self.display_busy,
                        egui::Button::new("创建虚拟显示").fill(accent()),
                    )
                    .clicked()
                {
                    self.create_display();
                }
                if ui.button("刷新列表").clicked() {
                    self.refresh_displays();
                }
                if self.display_busy {
                    ui.spinner();
                }
            });
            if let Some(err) = &self.display_error {
                ui.add_space(6.0);
                ui.colored_label(Color32::from_rgb(255, 160, 120), err);
            }
        });

        ui.add_space(16.0);
        ui.heading("现有显示");
        if self.displays.is_empty() && !self.display_busy {
            empty_state(
                ui,
                "没有虚拟显示",
                "可以创建一块，也可以让被控端在启动时自动创建。",
            );
        }
        let displays = self.displays.clone();
        let dynamic = self
            .display_backend
            .as_ref()
            .is_some_and(|backend| backend.capabilities() & capability::DYNAMIC_MODES != 0);
        for display in displays {
            card(ui, |ui| {
                ui.horizontal_top(|ui| {
                    ui.vertical(|ui| {
                        ui.label(RichText::new(format!("显示 #{}", display.id)).strong());
                        ui.label(format!(
                            "{}×{} · {} Hz · {} DPI",
                            display.spec.width,
                            display.spec.height,
                            display.spec.refresh_hz,
                            display.spec.dpi
                        ));
                        ui.label(if display.attached {
                            "已连接桌面"
                        } else {
                            "未连接桌面"
                        });
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("删除").clicked() {
                            self.request_confirmation(
                                "删除虚拟显示",
                                &format!("确定删除显示 #{} 吗？", display.id),
                                ConfirmAction::DestroyDisplay(display.id),
                            );
                        }
                        if ui
                            .add_enabled(dynamic, egui::Button::new("应用上方参数"))
                            .clicked()
                        {
                            self.update_display(display.id);
                        }
                        if !dynamic {
                            ui.label(RichText::new("不支持动态修改").small());
                        }
                    });
                });
            });
            ui.add_space(8.0);
        }
    }

    fn ui_diagnostics(&mut self, ui: &mut egui::Ui) {
        ui.heading("诊断");
        ui.label("这些检查只读取本机状态，不会启动远控会话或修改系统设置。");
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if ui.button("重新检查").clicked() {
                self.refresh_diagnostics();
            }
        });
        ui.add_space(8.0);
        self.refresh_activity_count();
        for diagnostic in &self.diagnostics {
            card(ui, |ui| {
                ui.horizontal_top(|ui| {
                    ui.label(
                        RichText::new(if diagnostic.ok { "✓" } else { "!" })
                            .strong()
                            .color(if diagnostic.ok {
                                Color32::from_rgb(116, 211, 152)
                            } else {
                                Color32::from_rgb(240, 166, 105)
                            }),
                    );
                    ui.vertical(|ui| {
                        ui.label(RichText::new(&diagnostic.name).strong());
                        ui.label(&diagnostic.detail);
                    });
                });
            });
            ui.add_space(8.0);
        }
        if !self.children.is_empty() {
            ui.add_space(8.0);
            ui.heading("活动进程");
            let children: Vec<(u64, String, String)> = self
                .children
                .iter()
                .map(|child| {
                    (
                        child.id,
                        child.label.clone(),
                        if child.last_output.is_empty() {
                            format!("PID {}", child.child.id())
                        } else {
                            child.last_output.clone()
                        },
                    )
                })
                .collect();
            for (id, label, detail) in children {
                card(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(label).strong());
                        ui.label(detail);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("停止").clicked() {
                                self.stop_child(id);
                            }
                        });
                    });
                });
                ui.add_space(8.0);
            }
        }
    }

    fn refresh_activity_count(&mut self) {
        if let Some(diagnostic) = self
            .diagnostics
            .iter_mut()
            .find(|diagnostic| diagnostic.name == "活动子进程")
        {
            diagnostic.detail = format!("{} 个 Viewer / Agent 正在运行", self.children.len());
        }
    }

    fn draw_modal(&mut self, ctx: &egui::Context) {
        let modal = std::mem::replace(&mut self.modal, Modal::None);
        let mut keep_open = true;
        match modal {
            Modal::None => {}
            Modal::Device(mut form) => {
                let title = if form.id.is_some() {
                    "编辑设备"
                } else {
                    "添加设备"
                };
                egui::Window::new(title)
                    .id(egui::Id::new("device-modal"))
                    .collapsible(false)
                    .resizable(false)
                    .default_width(480.0)
                    .show(ctx, |ui| {
                        form_grid(ui, "device-form", |ui| {
                            grid_label(ui, "名称", "device-name", |ui, width| {
                                ui.add_sized(
                                    [width, 0.0],
                                    egui::TextEdit::singleline(&mut form.name),
                                );
                            });
                            grid_label(ui, "地址", "device-addr", |ui, width| {
                                ui.add_sized(
                                    [width, 0.0],
                                    egui::TextEdit::singleline(&mut form.addr)
                                        .hint_text("192.168.1.20:21116"),
                                );
                            });
                            grid_label(ui, "中继 tag", "device-tag", |ui, width| {
                                ui.add_enabled(
                                    form.use_relay,
                                    egui::TextEdit::singleline(&mut form.relay_tag)
                                        .desired_width(width),
                                );
                            });
                            ui.label("使用中继");
                            ui.checkbox(&mut form.use_relay, "");
                            ui.end_row();
                            grid_label(
                                ui,
                                "证书指纹（可选）",
                                "device-fingerprint",
                                |ui, width| {
                                    ui.add_sized(
                                        [width, 0.0],
                                        egui::TextEdit::singleline(&mut form.fingerprint),
                                    );
                                },
                            );
                            ui.label("收藏");
                            ui.checkbox(&mut form.favorite, "");
                            ui.end_row();
                        });
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            if ui.button("保存").clicked() {
                                match self.save_device_form(&form) {
                                    Ok(()) => keep_open = false,
                                    Err(err) => self.set_status(err.to_string(), true),
                                }
                            }
                            if ui.button("取消").clicked() {
                                keep_open = false;
                            }
                        });
                    });
                if keep_open {
                    self.modal = Modal::Device(form);
                }
            }
            Modal::Relay(mut form) => {
                let title = if form.id.is_some() {
                    "编辑中继"
                } else {
                    "添加中继"
                };
                egui::Window::new(title)
                    .id(egui::Id::new("relay-modal"))
                    .collapsible(false)
                    .resizable(false)
                    .default_width(500.0)
                    .show(ctx, |ui| {
                        form_grid(ui, "relay-form", |ui| {
                            grid_label(ui, "名称", "relay-name", |ui, width| {
                                ui.add_sized(
                                    [width, 0.0],
                                    egui::TextEdit::singleline(&mut form.name),
                                );
                            });
                            grid_label(ui, "档案 ID", "relay-id", |ui, width| {
                                ui.add_enabled(
                                    form.id.is_none(),
                                    egui::TextEdit::singleline(&mut form.profile_id)
                                        .desired_width(width)
                                        .hint_text("留空自动生成"),
                                );
                            });
                            ui.end_row();
                            grid_label(ui, "主机地址", "relay-host", |ui, width| {
                                ui.add_sized(
                                    [width, 0.0],
                                    egui::TextEdit::singleline(&mut form.host)
                                        .hint_text("relay.example.com 或 IP"),
                                );
                            });
                            grid_label(ui, "端口", "relay-port", |ui, width| {
                                ui.add_sized(
                                    [width.min(120.0), 0.0],
                                    egui::TextEdit::singleline(&mut form.port),
                                );
                            });
                            ui.end_row();
                            grid_label(ui, "中继密钥", "relay-key", |ui, width| {
                                ui.add_sized(
                                    [width, 0.0],
                                    egui::TextEdit::singleline(form.key.get_mut())
                                        .password(true)
                                        .hint_text(if form.id.is_some() {
                                            "留空保留原密钥"
                                        } else {
                                            "hex:… / base64:…"
                                        }),
                                );
                            });
                            ui.label("只显示为掩码");
                            ui.end_row();
                            grid_label(ui, "区域（可选）", "relay-region", |ui, width| {
                                ui.add_sized(
                                    [width, 0.0],
                                    egui::TextEdit::singleline(&mut form.region),
                                );
                            });
                            grid_label(ui, "优先级", "relay-priority", |ui, width| {
                                ui.add_sized(
                                    [width.min(120.0), 0.0],
                                    egui::TextEdit::singleline(&mut form.priority),
                                );
                            });
                            ui.end_row();
                            ui.label("TLS 策略");
                            egui::ComboBox::from_id_source("relay-tls")
                                .selected_text(form.tls.as_str())
                                .show_ui(ui, |ui| {
                                    for tls in [TlsMode::Auto, TlsMode::Disabled, TlsMode::Required]
                                    {
                                        if ui
                                            .selectable_label(form.tls == tls, tls.as_str())
                                            .clicked()
                                        {
                                            form.tls = tls;
                                            ui.close_menu();
                                        }
                                    }
                                });
                            ui.end_row();
                            ui.label("首选服务器");
                            ui.checkbox(&mut form.preferred, "");
                            ui.end_row();
                        });
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            if ui.button("保存").clicked() {
                                match self.save_relay_form(&form) {
                                    Ok(()) => keep_open = false,
                                    Err(err) => self.set_status(err.to_string(), true),
                                }
                            }
                            if ui.button("取消").clicked() {
                                keep_open = false;
                            }
                        });
                    });
                if keep_open {
                    self.modal = Modal::Relay(form);
                }
            }
            Modal::Confirm(confirm) => {
                egui::Window::new(&confirm.title)
                    .id(egui::Id::new("confirm-modal"))
                    .collapsible(false)
                    .resizable(false)
                    .default_width(380.0)
                    .show(ctx, |ui| {
                        ui.label(&confirm.message);
                        ui.add_space(12.0);
                        ui.horizontal(|ui| {
                            if ui
                                .add(egui::Button::new("确认").fill(Color32::from_rgb(164, 67, 67)))
                                .clicked()
                            {
                                if let Err(err) = self.confirm_action(confirm.action.clone()) {
                                    self.set_status(err.to_string(), true);
                                }
                                keep_open = false;
                            }
                            if ui.button("取消").clicked() {
                                keep_open = false;
                            }
                        });
                    });
                if keep_open {
                    self.modal = Modal::Confirm(confirm);
                }
            }
        }
    }
}

fn strategy_label(strategy: SelectionStrategy) -> &'static str {
    match strategy {
        SelectionStrategy::Manual => "手动指定",
        SelectionStrategy::PreferredFirst => "首选优先",
        SelectionStrategy::LowestLatency => "最低延迟",
        SelectionStrategy::AutoFailover => "自动故障切换",
    }
}

fn accent() -> Color32 {
    Color32::from_rgb(48, 112, 190)
}

fn card(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    let width = ui.available_width();
    egui::Frame::none()
        .fill(Color32::from_rgb(30, 38, 51))
        .rounding(egui::Rounding::same(10.0))
        .inner_margin(egui::Margin::same(14.0))
        .show(ui, |ui| {
            ui.set_width(width - 28.0);
            add(ui);
        });
}

fn empty_state(ui: &mut egui::Ui, title: &str, detail: &str) {
    ui.add_space(12.0);
    ui.label(RichText::new(title).strong());
    ui.label(RichText::new(detail).color(Color32::from_rgb(150, 164, 184)));
    ui.add_space(12.0);
}

fn form_grid(ui: &mut egui::Ui, id: &str, add: impl FnOnce(&mut egui::Ui)) {
    egui::Grid::new(id)
        .num_columns(2)
        .spacing([14.0, 8.0])
        .show(ui, add);
}

fn grid_label(ui: &mut egui::Ui, label: &str, id: &str, add: impl FnOnce(&mut egui::Ui, f32)) {
    ui.label(RichText::new(label).strong());
    let width = (ui.available_width() - 8.0).clamp(140.0, 420.0);
    ui.push_id(id, |ui| add(ui, width));
    ui.end_row();
}

fn device_summary(ui: &mut egui::Ui, entry: &DeviceEntry) {
    ui.vertical(|ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new(&entry.name).strong());
            if entry.favorite {
                ui.label(RichText::new("★").color(Color32::from_rgb(235, 195, 92)));
            }
            if entry.use_relay {
                ui.label(RichText::new("中继").small());
            }
        });
        ui.label(
            RichText::new(&entry.addr)
                .small()
                .color(Color32::from_rgb(151, 166, 187)),
        );
        if entry.last_used_ms > 0 {
            ui.label(
                RichText::new("最近连接过")
                    .small()
                    .color(Color32::from_rgb(126, 139, 158)),
            );
        }
    });
}

fn configure_style(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = Color32::from_rgb(20, 26, 36);
    visuals.window_fill = Color32::from_rgb(27, 34, 46);
    visuals.extreme_bg_color = Color32::from_rgb(15, 20, 28);
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(38, 48, 64);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(50, 65, 86);
    visuals.widgets.active.bg_fill = Color32::from_rgb(48, 112, 190);
    visuals.widgets.noninteractive.bg_fill = Color32::from_rgb(30, 38, 51);
    visuals.widgets.noninteractive.fg_stroke.color = Color32::from_rgb(210, 218, 230);
    visuals.selection.bg_fill = Color32::from_rgb(48, 112, 190);
    visuals.window_rounding = egui::Rounding::same(10.0);
    ctx.set_visuals(visuals);
    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = Vec2::new(8.0, 7.0);
    style.spacing.button_padding = Vec2::new(10.0, 6.0);
    ctx.set_style(style);
}

fn install_fonts(ctx: &egui::Context, custom: Option<PathBuf>) {
    let mut fonts = FontDefinitions::default();
    let candidates: Vec<PathBuf> = custom.into_iter().collect();
    let candidates = if candidates.is_empty() {
        vec![
            PathBuf::from(r"C:\Windows\Fonts\simhei.ttf"),
            PathBuf::from(r"C:\Windows\Fonts\segoeui.ttf"),
            PathBuf::from("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc"),
            PathBuf::from("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"),
        ]
    } else {
        candidates
    };
    for path in candidates {
        if let Ok(data) = fs::read(&path) {
            let name = format!("manager-font-{}", fonts.font_data.len());
            fonts
                .font_data
                .insert(name.clone(), egui::FontData::from_owned(data));
            fonts
                .families
                .entry(FontFamily::Proportional)
                .or_default()
                .insert(0, name.clone());
            fonts
                .families
                .entry(FontFamily::Monospace)
                .or_default()
                .insert(0, name);
            break;
        }
    }
    ctx.set_fonts(fonts);
}

fn load_icon() -> IconData {
    const ICON_PNG: &[u8] = include_bytes!("../../../assets/icon-512.png");
    let mut decoder = png::Decoder::new(io::Cursor::new(ICON_PNG));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    if let Ok(mut reader) = decoder.read_info() {
        if let Some(size) = reader.output_buffer_size() {
            let mut buffer = vec![0u8; size];
            if let Ok(frame) = reader.next_frame(&mut buffer) {
                let bytes = &buffer[..frame.buffer_size()];
                let rgba = match frame.color_type {
                    png::ColorType::Rgba => bytes.to_vec(),
                    png::ColorType::Rgb => bytes
                        .chunks_exact(3)
                        .flat_map(|rgb| [rgb[0], rgb[1], rgb[2], 255])
                        .collect(),
                    _ => Vec::new(),
                };
                if !rgba.is_empty() {
                    return IconData {
                        rgba,
                        width: frame.width,
                        height: frame.height,
                    };
                }
            }
        }
    }
    IconData {
        rgba: vec![0; 64 * 64 * 4],
        width: 64,
        height: 64,
    }
}

fn main() -> eframe::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .try_init();
    let args = Args::parse();
    let font = args.font.clone();
    let app = ManagerApp::new(args);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_TITLE)
            .with_inner_size([1120.0, 720.0])
            .with_min_inner_size([760.0, 520.0])
            .with_icon(load_icon()),
        ..Default::default()
    };
    eframe::run_native(
        APP_TITLE,
        options,
        Box::new(move |ctx| {
            configure_style(&ctx.egui_ctx);
            install_fonts(&ctx.egui_ctx, font);
            Box::new(app) as Box<dyn eframe::App>
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "dfy-manager-{name}-{}-{}",
            std::process::id(),
            random_suffix(6)
        ))
    }

    #[test]
    fn device_book_roundtrip_and_order() {
        let path = temp_file("devices.toml");
        let mut book = DeviceBook::load(&path).unwrap();
        let mut favorite = DeviceEntry::new("收藏设备".into(), "127.0.0.1:21116".into());
        favorite.favorite = true;
        favorite.last_used_ms = 1;
        book.upsert(favorite);
        book.upsert(DeviceEntry::new(
            "普通设备".into(),
            "127.0.0.2:21116".into(),
        ));
        book.save().unwrap();
        let loaded = DeviceBook::load(&path).unwrap();
        assert_eq!(loaded.devices.len(), 2);
        assert_eq!(loaded.devices[0].name, "收藏设备");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn corrupted_device_book_is_not_silently_accepted() {
        let path = temp_file("broken.toml");
        fs::write(&path, "not = [valid").unwrap();
        assert!(DeviceBook::load(&path).is_err());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn password_validation_matches_cli_contract() {
        assert!(validate_password("1234567890123456").is_ok());
        assert!(validate_password("short").is_err());
        assert!(validate_password("                ").is_err());
        assert!(validate_password(&"x".repeat(1025)).is_err());
        let password = generate_password();
        assert_eq!(password.len(), 24);
        assert!(validate_password(&password).is_ok());
    }

    #[test]
    fn viewer_command_never_puts_password_in_arguments() {
        let request = ViewerRequest {
            label: "test".into(),
            addr: "127.0.0.1:21116".into(),
            use_relay: false,
            relay_tag: String::new(),
            fingerprint: String::new(),
            clipboard: true,
            device_id: None,
        };
        let command =
            view_command(Path::new("dfy"), &request, Path::new("relay.toml"), None).unwrap();
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(args.iter().any(|arg| arg == "--password-stdin"));
        assert!(args.iter().any(|arg| arg == "--clipboard"));
        assert!(!args.iter().any(|arg| arg == "--password"));
    }

    #[test]
    fn relay_connection_uses_profile_without_exposing_key() {
        let request = ViewerRequest {
            label: "test".into(),
            addr: "127.0.0.1:21116".into(),
            use_relay: true,
            relay_tag: "ab".repeat(16),
            fingerprint: String::new(),
            clipboard: false,
            device_id: None,
        };
        let command = view_command(
            Path::new("dfy"),
            &request,
            Path::new("relay.toml"),
            Some("office"),
        )
        .unwrap();
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--relay-profile", "office"]));
        assert!(!args.iter().any(|arg| arg == "--relay-key"));
    }

    #[test]
    fn relay_tag_and_fingerprint_are_validated() {
        assert!(validate_relay_tag(&"ab".repeat(16)).is_ok());
        assert!(validate_relay_tag("abc").is_err());
        assert!(validate_fingerprint(&"cd".repeat(32)).is_ok());
        assert!(validate_fingerprint(&"cd".repeat(31)).is_err());
    }

    #[test]
    fn display_draft_rejects_odd_dimensions() {
        let mut draft = DisplayDraft {
            width: "1921".into(),
            ..DisplayDraft::default()
        };
        assert!(draft.spec().is_err());
        draft.width = "1920".into();
        assert!(draft.spec().is_ok());
    }
}
