// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (C) 2025 Dafeiyu Remote Project

//! No-root Linux virtual desktops backed by isolated Xvfb servers.
//!
//! Each display has its own X server, UNIX socket, lock and process. This is an X11
//! desktop suitable for headless capture/automation, **not** an EVDI/DRM connector:
//! Wayland compositors and physical-display topology are deliberately untouched.
//! Consumers should use [`XvfbBackend::display_name`] to obtain the managed `:N`
//! endpoint and pass it to X11 capture/input clients.
//!
//! A private, per-user registry serialises allocation across backend instances. Every
//! record includes the server PID and `/proc` start time, and cleanup verifies both the
//! record and Xvfb's command line before signalling a process. Foreign X locks/sockets
//! are never removed. Creation and recovery return success only after `xdpyinfo` has
//! completed an actual X handshake and reported the requested geometry.
//!
//! Xvfb has no portable run-time refresh/DPI control. `update` therefore supports only
//! framebuffer geometry. It first tries `xrandr --fb` and verifies the result with
//! `xdpyinfo`; Xvfb builds commonly permit shrinking but reject enlargement. If the
//! measured geometry did not change, the backend recreates only that display's server
//! with the same public id. Clients connected to the old X server must reconnect after
//! such a fallback. Refresh/DPI changes fail explicitly as unsupported. Refresh and DPI
//! values in a spec are retained as caller metadata, not measured Xvfb modes.

use std::collections::HashMap;
use std::ffi::{c_int, c_long, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};

use crate::{
    capability, validate_spec, VirtualDisplayBackend, VirtualDisplayInfo, VirtualDisplaySpec,
};

const START_TIMEOUT: Duration = Duration::from_secs(5);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
const HEALTH_TIMEOUT: Duration = Duration::from_millis(750);
const STOP_TIMEOUT: Duration = Duration::from_secs(1);
const FIRST_DISPLAY: u32 = 100;
const DISPLAY_COUNT: u32 = 900;
const START_ATTEMPTS: u32 = 8;

static NEXT_ID_SEED: AtomicU32 = AtomicU32::new(1);

/// Linux backend managing one isolated Xvfb process per virtual desktop.
pub struct XvfbBackend {
    root: PathBuf,
    instance: PathBuf,
    state: Mutex<BackendState>,
}

#[derive(Default)]
struct BackendState {
    /// Handles for servers spawned by this instance. Keeping them also prevents PID
    /// reuse while they are running and lets Drop reap them instead of making zombies.
    children: HashMap<u32, Child>,
}

/// An on-disk display record. It is intentionally a small dependency-free format.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Record {
    id: u32,
    display: u32,
    spec: VirtualDisplaySpec,
    pid: u32,
    start_ticks: u64,
    instance: String,
}

impl Record {
    fn encode(&self) -> String {
        // validate_spec has already excluded newlines and non-ASCII name hints.
        format!(
            "format=1\nid={}\ndisplay={}\nwidth={}\nheight={}\nrefresh_hz={}\ndpi={}\nname_hint={}\npid={}\nstart_ticks={}\ninstance={}\n",
            self.id,
            self.display,
            self.spec.width,
            self.spec.height,
            self.spec.refresh_hz,
            self.spec.dpi,
            self.spec.name_hint,
            self.pid,
            self.start_ticks,
            self.instance
        )
    }

    fn parse(text: &str) -> Result<Self> {
        let mut values = HashMap::new();
        for line in text.lines() {
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| anyhow!("malformed registry line"))?;
            if values.insert(key.to_string(), value.to_string()).is_some() {
                bail!("duplicate registry key {key}");
            }
        }
        let number = |key: &str| -> Result<u32> {
            values
                .get(key)
                .ok_or_else(|| anyhow!("missing registry key {key}"))?
                .parse()
                .with_context(|| format!("invalid registry key {key}"))
        };
        if values.get("format").map(String::as_str) != Some("1") {
            bail!("unsupported display registry format");
        }
        let record = Self {
            id: number("id")?,
            display: number("display")?,
            spec: VirtualDisplaySpec::new(
                number("width")?,
                number("height")?,
                number("refresh_hz")?,
                number("dpi")?,
                values
                    .get("name_hint")
                    .ok_or_else(|| anyhow!("missing registry key name_hint"))?
                    .clone(),
            ),
            pid: number("pid")?,
            start_ticks: values
                .get("start_ticks")
                .ok_or_else(|| anyhow!("missing registry key start_ticks"))?
                .parse()?,
            instance: values
                .get("instance")
                .ok_or_else(|| anyhow!("missing registry key instance"))?
                .clone(),
        };
        validate_spec(&record.spec)?;
        if record.id == 0 || record.pid == 0 || record.start_ticks == 0 {
            bail!("display record has a zero id, pid or start time");
        }
        if !valid_instance_name(&record.instance) {
            bail!("display record has an invalid instance name");
        }
        Ok(record)
    }

    fn directory(&self, root: &Path) -> PathBuf {
        root.join(format!("display-{}", self.id))
    }

    fn info(&self, attached: bool) -> VirtualDisplayInfo {
        VirtualDisplayInfo {
            id: self.id,
            spec: self.spec.clone(),
            attached,
        }
    }

    fn owns_x_endpoint(&self) -> bool {
        self.is_owned_server() && x_artifacts_owned(self)
    }

    /// Checks PID start time and the exact managed-server arguments before any signal.
    fn is_owned_server(&self) -> bool {
        if !process_running(self.pid, self.start_ticks) {
            return false;
        }
        let Ok(raw) = fs::read(format!("/proc/{}/cmdline", self.pid)) else {
            return false;
        };
        let args: Vec<OsString> = raw
            .split(|byte| *byte == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| OsString::from_vec(arg.to_vec()))
            .collect();
        let Some(program) = args.first().and_then(|arg| Path::new(arg).file_name()) else {
            return false;
        };
        if program != "Xvfb" {
            return false;
        }
        args.windows(3).any(|parts| {
            parts[0] == "-screen" && parts[1] == "0" && parts[2].to_string_lossy().ends_with("x24")
        }) && args
            .iter()
            .any(|arg| Path::new(arg) == Path::new(display_name(self.display).as_str()))
    }
}

/// File-descriptor lock serialising registry changes between backend processes.
struct RegistryGuard {
    file: File,
}

impl RegistryGuard {
    fn acquire(root: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(O_NOFOLLOW)
            .open(root.join(".registry.lock"))
            .context("open virtual display registry lock")?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.uid() != current_uid() {
            bail!("virtual display registry lock is not a private file owned by this user");
        }
        // SAFETY: `file` owns a valid descriptor for the duration of the call. flock is
        // process-associated on Linux; the descriptor remains open until after unlock.
        if unsafe { flock(file.as_raw_fd(), LOCK_EX) } != 0 {
            bail!(
                "lock virtual display registry: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(Self { file })
    }
}

impl Drop for RegistryGuard {
    fn drop(&mut self) {
        // SAFETY: the descriptor is still valid and no other thread can acquire this guard.
        unsafe {
            flock(self.file.as_raw_fd(), LOCK_UN);
        }
    }
}

impl XvfbBackend {
    /// Creates a backend without probing or opening any X display.
    ///
    /// The private registry is created here; Xvfb is started only by `create`/recovery.
    pub fn new() -> Result<Self> {
        let root = registry_root()?;
        create_private_dir(&root)?;
        let _ = RegistryGuard::acquire(&root)?;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let instance_name = format!("instance-{}-{nonce:x}", std::process::id());
        let instance = root.join(&instance_name);
        create_private_dir(&instance)?;
        Ok(Self {
            root,
            instance,
            state: Mutex::new(BackendState::default()),
        })
    }

    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, BackendState>> {
        self.state
            .lock()
            .map_err(|_| anyhow!("Xvfb backend state is poisoned"))
    }

    fn instance_name(&self) -> String {
        self.instance
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    }

    fn registry(&self) -> Result<RegistryGuard> {
        RegistryGuard::acquire(&self.root)
    }

    /// Returns the X11 endpoint (for example `:107`) for a live, owned display.
    ///
    /// This performs a health handshake and never returns the caller's physical DISPLAY.
    pub fn display_name(&self, id: u32) -> Result<String> {
        let _registry = self.registry()?;
        let record = self.find_record(id)?;
        ensure_healthy(&record)?;
        Ok(display_name(record.display))
    }

    /// Returns the PID of the managed Xvfb process after verifying its ownership.
    ///
    /// Useful for diagnostics; callers must not use this to signal arbitrary processes.
    pub fn server_pid(&self, id: u32) -> Result<u32> {
        let _registry = self.registry()?;
        let record = self.find_record(id)?;
        if !record.owns_x_endpoint() {
            bail!("virtual display {id} has no live owned Xvfb process");
        }
        Ok(record.pid)
    }

    /// Recreates an unhealthy server while preserving its public display id and spec.
    pub fn recover(&self, id: u32) -> Result<()> {
        let mut state = self.lock_state()?;
        let _registry = self.registry()?;
        let record = self.find_record(id)?;
        let _ = self.recreate_locked(&mut state, &record, &record.spec)?;
        Ok(())
    }

    /// Removes dead servers and stale registry/socket artifacts belonging to this backend.
    ///
    /// Other users' files and all foreign/live X servers are left untouched. Live orphaned
    /// servers recorded by an earlier backend instance are adopted, not killed.
    pub fn cleanup_stale(&self) -> Result<()> {
        let mut state = self.lock_state()?;
        let _registry = self.registry()?;
        cleanup_stale_locked(&self.root, &self.instance_name(), &mut state)
    }

    fn find_record(&self, id: u32) -> Result<Record> {
        let path = self.root.join(format!("display-{id}"));
        read_record(&path)?.with_context(|| format!("unknown virtual display id {id}"))
    }

    fn read_records(&self) -> Result<Vec<(PathBuf, Record)>> {
        read_records(&self.root)
    }

    fn recreate_locked(
        &self,
        state: &mut BackendState,
        old: &Record,
        spec: &VirtualDisplaySpec,
    ) -> Result<(Record, Child)> {
        terminate_record(state, old, STOP_TIMEOUT)?;
        remove_x_artifacts(old)?;
        remove_reservation(
            &self.root,
            old.display,
            &old.directory(&self.root)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy(),
        )?;
        self.start_locked(spec, Some(old.display), old.id, true)
    }

    fn start_locked(
        &self,
        spec: &VirtualDisplaySpec,
        preferred_display: Option<u32>,
        id: u32,
        recreating: bool,
    ) -> Result<(Record, Child)> {
        preflight()?;
        let seed = NEXT_ID_SEED.fetch_add(DISPLAY_COUNT / 4, Ordering::Relaxed);
        let first = preferred_display.unwrap_or(FIRST_DISPLAY + seed % DISPLAY_COUNT);
        let mut last_error = None;
        for offset in 0..START_ATTEMPTS {
            let display = FIRST_DISPLAY + (first - FIRST_DISPLAY + offset) % DISPLAY_COUNT;
            if !display_available(&self.root, display)? {
                continue;
            }
            let directory = self.root.join(format!("display-{id}"));
            if !recreating {
                create_private_dir(&directory)?;
            } else if !directory.is_dir() {
                bail!("display {id} registry directory disappeared during recovery");
            }
            let dir_name = directory
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            match reserve_display(&self.root, display, &dir_name) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error).context("reserve X display number"),
            }
            match spawn_server(&directory, id, display, spec, &self.instance_name()) {
                Ok(server) => return Ok(server),
                Err(error) => {
                    let _ = remove_reservation(&self.root, display, &dir_name);
                    // A failed recreation retains the old record/spec for a later retry.
                    if !recreating {
                        let _ = fs::remove_dir_all(&directory);
                    }
                    last_error = Some(error);
                }
            }
        }
        Err(last_error.unwrap_or_else(|| {
            anyhow!(
                "no free X display number in {FIRST_DISPLAY}..={}",
                FIRST_DISPLAY + DISPLAY_COUNT - 1
            )
        }))
    }
}

impl Drop for XvfbBackend {
    fn drop(&mut self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let Ok(_registry) = self.registry() else {
            return;
        };
        let instance = self.instance_name();
        let records = read_records(&self.root).unwrap_or_default();
        for (_, record) in records {
            if record.instance != instance {
                continue;
            }
            if terminate_record(&mut state, &record, STOP_TIMEOUT).is_ok() {
                let name = record
                    .directory(&self.root)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                let _ = remove_x_artifacts(&record);
                let _ = remove_reservation(&self.root, record.display, &name);
                let _ = fs::remove_dir_all(record.directory(&self.root));
            }
        }
        let _ = fs::remove_dir(&self.instance);
    }
}

impl VirtualDisplayBackend for XvfbBackend {
    fn capture_endpoint(&self, id: u32) -> Result<Option<String>> {
        Ok(Some(self.display_name(id)?))
    }

    fn list(&self) -> Result<Vec<VirtualDisplayInfo>> {
        let mut state = self.lock_state()?;
        let _registry = self.registry()?;
        cleanup_stale_locked(&self.root, &self.instance_name(), &mut state)?;
        let mut result = Vec::new();
        for (_, record) in self.read_records()? {
            reap_child(&mut state, &record);
            if record.owns_x_endpoint()
                && query_geometry(record.display, HEALTH_TIMEOUT)
                    .is_ok_and(|size| size == (record.spec.width, record.spec.height))
            {
                result.push(record.info(true));
                continue;
            }
            match self.recreate_locked(&mut state, &record, &record.spec) {
                Ok((recovered, child)) => {
                    state.children.insert(recovered.display, child);
                    result.push(recovered.info(true));
                }
                Err(_) => result.push(record.info(false)),
            }
        }
        Ok(result)
    }

    fn create(&self, spec: &VirtualDisplaySpec) -> Result<u32> {
        spec.validate()?;
        let mut state = self.lock_state()?;
        let _registry = self.registry()?;
        cleanup_stale_locked(&self.root, &self.instance_name(), &mut state)?;
        // Allocate the id once, then retain it if a racy X server occupies our first
        // candidate. Failed attempts may consume ids; ids are never intentionally reused.
        let id = allocate_id(&self.root)?;
        let (created, child) = self.start_locked(spec, None, id, false)?;
        state.children.insert(created.display, child);
        Ok(created.id)
    }

    fn update(&self, id: u32, spec: &VirtualDisplaySpec) -> Result<()> {
        let mut state = self.lock_state()?;
        let _registry = self.registry()?;
        let mut record = self.find_record(id)?;
        spec.validate()?;
        if spec.refresh_hz != record.spec.refresh_hz || spec.dpi != record.spec.dpi {
            bail!(
                "unsupported Xvfb mode change for display {id}: refresh rate and DPI are fixed \
                 at server creation (requested {} Hz/{} DPI, current {} Hz/{} DPI)",
                spec.refresh_hz,
                spec.dpi,
                record.spec.refresh_hz,
                record.spec.dpi
            );
        }
        // The friendly name is fixed at creation, as required by the trait contract.
        let mut updated = spec.clone();
        updated.name_hint.clone_from(&record.spec.name_hint);
        reap_child(&mut state, &record);
        let measured = if record.owns_x_endpoint() {
            query_geometry(record.display, HEALTH_TIMEOUT).ok()
        } else {
            None
        };
        if needs_recreation(
            record.owns_x_endpoint(),
            measured,
            (record.spec.width, record.spec.height),
        ) {
            let (healthy, child) = self.recreate_locked(&mut state, &record, &record.spec)?;
            state.children.insert(healthy.display, child);
            record = healthy;
        }
        ensure_healthy(&record)?;

        // Some Xvfb/RandR builds return BadValue for a shrink even though the framebuffer
        // did change. The measured X handshake, not xrandr's exit status, is authoritative.
        let live_result = if command_available("xrandr") {
            run_command_impl(
                &mut xrandr_fb_command(record.display, updated.width, updated.height),
                COMMAND_TIMEOUT,
                true,
            )
            .and_then(|_| query_geometry(record.display, COMMAND_TIMEOUT))
        } else {
            Err(anyhow!("xrandr is not installed"))
        };
        if live_result.as_ref().ok() == Some(&(updated.width, updated.height)) {
            record.spec = updated;
            write_record(&record.directory(&self.root).join("record"), &record)?;
        } else {
            // Enlargement beyond Xvfb's initial framebuffer (or any unsupported RandR
            // operation) recreates this display only. spawn_server's handshake verifies
            // the new size before this reports success.
            let (recreated, child) = self.recreate_locked(&mut state, &record, &updated)?;
            state.children.insert(recreated.display, child);
        }
        Ok(())
    }

    fn destroy(&self, id: u32) -> Result<()> {
        let mut state = self.lock_state()?;
        let _registry = self.registry()?;
        let record = self.find_record(id)?;
        terminate_record(&mut state, &record, STOP_TIMEOUT)?;
        remove_x_artifacts(&record)?;
        let name = record
            .directory(&self.root)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        remove_reservation(&self.root, record.display, &name)?;
        fs::remove_dir_all(record.directory(&self.root))
            .with_context(|| format!("remove registry directory for display {id}"))?;
        Ok(())
    }

    fn is_available(&self) -> bool {
        command_available("Xvfb") && command_available("xdpyinfo")
    }

    fn capabilities(&self) -> u32 {
        // Geometry updates work, but may require server recreation; do not claim the
        // stronger live resolution/refresh/DPI contract of DYNAMIC_MODES.
        capability::MULTI_MONITOR
    }
}

fn preflight() -> Result<()> {
    for program in ["Xvfb", "xdpyinfo"] {
        if !command_available(program) {
            bail!("Linux virtual displays require {program} on PATH (install xvfb and x11-utils)");
        }
    }
    Ok(())
}

fn ensure_healthy(record: &Record) -> Result<()> {
    if !record.owns_x_endpoint() {
        bail!(
            "virtual display {} has no live owned Xvfb process",
            record.id
        );
    }
    if query_geometry(record.display, HEALTH_TIMEOUT)? != (record.spec.width, record.spec.height) {
        bail!(
            "virtual display {} failed its X11 health handshake",
            record.id
        );
    }
    Ok(())
}

fn spawn_server(
    directory: &Path,
    id: u32,
    display: u32,
    spec: &VirtualDisplaySpec,
    instance: &str,
) -> Result<(Record, Child)> {
    let mut child = xvfb_command(display, spec)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("start Xvfb")?;
    let pid = child.id();
    let start_ticks = match process_start_ticks(pid) {
        Ok(Some(ticks)) => ticks,
        Ok(None) => {
            terminate_child(&mut child, STOP_TIMEOUT)?;
            bail!("Xvfb exited before its process identity could be read");
        }
        Err(error) => {
            let _ = terminate_child(&mut child, STOP_TIMEOUT);
            return Err(error);
        }
    };
    let record = Record {
        id,
        display,
        spec: spec.clone(),
        pid,
        start_ticks,
        instance: instance.to_string(),
    };
    if let Err(error) = wait_until_ready(&mut child, &record, directory) {
        // wait_until_ready has stopped the child on every failure path. Remove only
        // artifacts whose standard lock still names this exact process incarnation.
        remove_x_artifacts(&record)?;
        return Err(error);
    }
    Ok((record, child))
}

fn wait_until_ready(child: &mut Child, record: &Record, directory: &Path) -> Result<()> {
    let display = record.display;
    let spec = &record.spec;
    let started = Instant::now();
    let mut last_probe_error = None;
    loop {
        if started.elapsed() >= START_TIMEOUT {
            terminate_child(child, STOP_TIMEOUT)?;
            bail!(
                "Xvfb :{display} did not accept X11 connections within {} ms{}",
                START_TIMEOUT.as_millis(),
                last_probe_error
                    .map(|error| format!(" (last probe: {error})"))
                    .unwrap_or_default()
            );
        }
        if let Some(status) = child.try_wait().context("check Xvfb process")? {
            child.wait()?;
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            if let Some(mut pipe) = child.stdout.take() {
                pipe.read_to_end(&mut stdout)?;
            }
            if let Some(mut pipe) = child.stderr.take() {
                pipe.read_to_end(&mut stderr)?;
            }
            bail!(
                "Xvfb :{} exited with {status}: {}",
                display,
                String::from_utf8_lossy(&stderr).trim()
            );
        }
        match query_geometry(
            display,
            START_TIMEOUT
                .saturating_sub(started.elapsed())
                .min(COMMAND_TIMEOUT),
        ) {
            Ok(size) if size == (spec.width, spec.height) => {
                if !x_artifacts_owned(record) {
                    terminate_child(child, STOP_TIMEOUT)?;
                    bail!(
                        "Xvfb :{} accepted a connection but did not create its own X lock/socket",
                        display
                    );
                }
                if let Err(error) = write_record(&directory.join("record"), record) {
                    terminate_child(child, STOP_TIMEOUT)?;
                    return Err(error);
                }
                return Ok(());
            }
            Ok(size) => {
                terminate_child(child, STOP_TIMEOUT)?;
                bail!(
                    "Xvfb :{} reports {}x{}, expected {}x{}",
                    display,
                    size.0,
                    size.1,
                    spec.width,
                    spec.height
                );
            }
            Err(error) => {
                last_probe_error = Some(error);
                std::thread::sleep(Duration::from_millis(40));
            }
        }
    }
}

fn xvfb_command(display: u32, spec: &VirtualDisplaySpec) -> Command {
    let mut command = Command::new("Xvfb");
    command.args([
        OsString::from(display_name(display)),
        OsString::from("-nolisten"),
        OsString::from("tcp"),
        OsString::from("-noreset"),
        OsString::from("-screen"),
        OsString::from("0"),
        OsString::from(format!("{}x{}x24", spec.width, spec.height)),
        // Xvfb only accepts DPI at startup (no runtime XRandR DPI control). Passing it
        // here makes the spec's DPI real and observable via `xdpyinfo`, not just metadata.
        OsString::from("-dpi"),
        OsString::from(spec.dpi.to_string()),
    ]);
    // Never inherit DISPLAY: even a mistakenly constructed child command must target
    // this backend's private server, not the user's physical X desktop.
    command.env("DISPLAY", display_name(display));
    command
}

fn xdpyinfo_command(display: u32) -> Command {
    let mut command = Command::new("xdpyinfo");
    command.arg("-display").arg(display_name(display));
    command.env("DISPLAY", display_name(display));
    command
}

fn xrandr_fb_command(display: u32, width: u32, height: u32) -> Command {
    let mut command = Command::new("xrandr");
    command
        .arg("--display")
        .arg(display_name(display))
        .arg("--fb")
        .arg(format!("{width}x{height}"));
    command.env("DISPLAY", display_name(display));
    command
}

fn display_name(number: u32) -> String {
    format!(":{number}")
}

fn command_available(program: &str) -> bool {
    if program.contains('/') {
        return Path::new(program).is_file();
    }
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|directory| {
        let metadata = match fs::metadata(directory.join(program)) {
            Ok(metadata) => metadata,
            Err(_) => return false,
        };
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    })
}

fn query_geometry(display: u32, timeout: Duration) -> Result<(u32, u32)> {
    let output = run_command(&mut xdpyinfo_command(display), timeout)?;
    parse_xdpyinfo_dimensions(&String::from_utf8_lossy(&output.stdout))
        .ok_or_else(|| anyhow!("xdpyinfo succeeded for :{display} but returned no dimensions"))
}

fn parse_xdpyinfo_dimensions(text: &str) -> Option<(u32, u32)> {
    let line = text
        .lines()
        .find(|line| line.trim_start().starts_with("dimensions:"))?;
    let token = line.split_whitespace().nth(1)?;
    let (width, height) = token.split_once('x')?;
    Some((width.parse().ok()?, height.parse().ok()?))
}

fn run_command(command: &mut Command, timeout: Duration) -> Result<Output> {
    run_command_impl(command, timeout, false)
}

fn run_command_impl(
    command: &mut Command,
    timeout: Duration,
    allow_failure: bool,
) -> Result<Output> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("start {}", command.get_program().to_string_lossy()))?;
    let started = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            let output = child.wait_with_output()?;
            if !output.status.success() && !allow_failure {
                bail!(
                    "{} failed with {}: {}",
                    command.get_program().to_string_lossy(),
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            return Ok(output);
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let output = child.wait_with_output()?;
            bail!(
                "{} timed out after {} ms: {}",
                command.get_program().to_string_lossy(),
                timeout.as_millis(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn process_start_ticks(pid: u32) -> Result<Option<u64>> {
    let path = format!("/proc/{pid}/stat");
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {path}")),
    };
    Ok(parse_proc_start_ticks(&text))
}

fn parse_proc_start_ticks(stat: &str) -> Option<u64> {
    // comm (field 2) may contain spaces or ')'. Fields after its final ')' start at
    // field 3 (state); starttime is field 22, hence index 19 in this suffix.
    let suffix = stat.rsplit_once(')')?.1;
    suffix.split_whitespace().nth(19)?.parse().ok()
}

fn process_running(pid: u32, start_ticks: u64) -> bool {
    process_start_ticks(pid)
        .ok()
        .flatten()
        .is_some_and(|ticks| ticks == start_ticks)
        && fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| parse_proc_start_ticks(&stat).map(|_| proc_state(&stat)))
            .is_some_and(|state| state != 'Z')
}

fn proc_state(stat: &str) -> char {
    stat.rsplit_once(')')
        .and_then(|(_, suffix)| suffix.split_whitespace().next())
        .and_then(|state| state.chars().next())
        .unwrap_or('?')
}

fn terminate_record(state: &mut BackendState, record: &Record, timeout: Duration) -> Result<()> {
    if let Some(child) = state.children.get_mut(&record.display) {
        if child.id() == record.pid {
            return terminate_child(child, timeout);
        }
        state.children.remove(&record.display);
    }
    if !record.is_owned_server() {
        return Ok(());
    }
    // An adopted process is not our Child, so use a pidfd. Unlike kill(pid), this cannot
    // accidentally hit a recycled PID if the original exits during cleanup.
    let pidfd = match PidFd::open(record.pid) {
        Ok(pidfd) => pidfd,
        Err(_) if !record.is_owned_server() => return Ok(()),
        Err(error) => return Err(error),
    };
    pidfd.signal(SIGTERM)?;
    let started = Instant::now();
    while record.is_owned_server() && started.elapsed() < timeout {
        std::thread::sleep(Duration::from_millis(25));
    }
    if record.is_owned_server() {
        pidfd.signal(SIGKILL)?;
    }
    Ok(())
}

fn terminate_child(child: &mut Child, timeout: Duration) -> Result<()> {
    if child.try_wait()?.is_none() {
        // The unreaped Child pins its PID, so this cannot signal a recycled process.
        // SAFETY: pid is the direct unreaped child of this process.
        if unsafe { kill(child.id() as c_int, SIGTERM) } != 0 {
            let _ = child.kill();
        }
        let started = Instant::now();
        while child.try_wait()?.is_none() && started.elapsed() < timeout {
            std::thread::sleep(Duration::from_millis(25));
        }
        if child.try_wait()?.is_none() {
            child.kill().context("kill unresponsive Xvfb process")?;
        }
    }
    child.wait().context("reap Xvfb process")?;
    Ok(())
}

fn reap_child(state: &mut BackendState, record: &Record) {
    if let Some(child) = state.children.get_mut(&record.display) {
        if child.id() == record.pid && child.try_wait().ok().flatten().is_some() {
            state.children.remove(&record.display);
        }
    }
}

struct PidFd(c_int);

impl PidFd {
    fn open(pid: u32) -> Result<Self> {
        let number = pidfd_open_syscall_number().ok_or_else(|| {
            anyhow!("safe cleanup of adopted Xvfb processes requires Linux pidfd support")
        })?;
        // SAFETY: pid is only an identifier; the kernel pins the referenced process in
        // the returned descriptor. No pointer arguments are passed.
        let fd = unsafe { syscall(number, pid as c_long, 0 as c_long) } as c_int;
        if fd < 0 {
            bail!(
                "pidfd_open({pid}) failed: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(Self(fd))
    }

    fn signal(&self, signal: c_int) -> Result<()> {
        let number = pidfd_send_signal_syscall_number()
            .ok_or_else(|| anyhow!("pidfd signal is unsupported on this architecture"))?;
        // SAFETY: self owns a live pidfd; null pointer and zero flags are valid for
        // pidfd_send_signal and target only the process pinned when the fd was opened.
        if unsafe {
            syscall(
                number,
                self.0 as c_long,
                signal as c_long,
                0 as c_long,
                0 as c_long,
            )
        } < 0
        {
            bail!(
                "pidfd_send_signal failed: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(())
    }
}

impl Drop for PidFd {
    fn drop(&mut self) {
        // SAFETY: this is the sole owner of the descriptor.
        unsafe {
            close(self.0);
        }
    }
}

#[cfg(target_arch = "x86_64")]
fn pidfd_open_syscall_number() -> Option<c_long> {
    Some(434)
}
#[cfg(target_arch = "aarch64")]
fn pidfd_open_syscall_number() -> Option<c_long> {
    Some(434)
}
#[cfg(target_arch = "x86")]
fn pidfd_open_syscall_number() -> Option<c_long> {
    Some(438)
}
#[cfg(target_arch = "riscv64")]
fn pidfd_open_syscall_number() -> Option<c_long> {
    Some(434)
}
#[cfg(not(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "x86",
    target_arch = "riscv64"
)))]
fn pidfd_open_syscall_number() -> Option<c_long> {
    None
}

#[cfg(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "x86",
    target_arch = "riscv64"
))]
fn pidfd_send_signal_syscall_number() -> Option<c_long> {
    Some(424)
}
#[cfg(not(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "x86",
    target_arch = "riscv64"
)))]
fn pidfd_send_signal_syscall_number() -> Option<c_long> {
    None
}

const O_NOFOLLOW: c_int = 0x2_0000;
const LOCK_EX: c_int = 2;
const LOCK_UN: c_int = 8;
const SIGTERM: c_int = 15;
const SIGKILL: c_int = 9;

extern "C" {
    fn flock(fd: c_int, operation: c_int) -> c_int;
    fn kill(pid: c_int, signal: c_int) -> c_int;
    fn close(fd: c_int) -> c_int;
    fn syscall(number: c_long, ...) -> c_long;
}

fn current_uid() -> u32 {
    // SAFETY: getuid has no arguments and cannot fail.
    unsafe { getuid() }
}
extern "C" {
    fn getuid() -> u32;
}

fn registry_root() -> Result<PathBuf> {
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").filter(|value| !value.is_empty()) {
        let runtime = PathBuf::from(runtime);
        if !runtime.is_absolute() {
            bail!("XDG_RUNTIME_DIR must be an absolute path");
        }
        return Ok(runtime.join("dfy-vdisplay"));
    }
    Ok(std::env::temp_dir().join(format!("dfy-vdisplay-{}", current_uid())))
}

fn create_private_dir(path: &Path) -> Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).with_context(|| format!("create {}", path.display())),
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.uid() != current_uid() || metadata.mode() & 0o077 != 0 {
        bail!(
            "{} must be a private directory owned by this user",
            path.display()
        );
    }
    Ok(())
}

fn valid_instance_name(name: &str) -> bool {
    !name.is_empty()
        && name.starts_with("instance-")
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn read_record(directory: &Path) -> Result<Option<Record>> {
    let path = directory.join("record");
    match fs::read_to_string(&path) {
        Ok(text) => Record::parse(&text).map(Some),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

fn write_record(path: &Path, record: &Record) -> Result<()> {
    let temporary = path.with_extension("new");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(O_NOFOLLOW)
        .open(&temporary)
        .with_context(|| format!("create {}", temporary.display()))?;
    file.write_all(record.encode().as_bytes())?;
    file.sync_all()?;
    fs::rename(&temporary, path).with_context(|| format!("install {}", path.display()))?;
    Ok(())
}

fn read_records(root: &Path) -> Result<Vec<(PathBuf, Record)>> {
    let mut records = Vec::new();
    for entry in fs::read_dir(root).context("read virtual display registry")? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with("display-") {
            continue;
        }
        // Never follow registry symlinks, even though the parent is private.
        if !fs::symlink_metadata(&path)?.is_dir() {
            continue;
        }
        match read_record(&path)? {
            Some(record) if record.directory(root) == path => records.push((path, record)),
            Some(_) => bail!("display registry record {name} contains a mismatched id"),
            None => {}
        }
    }
    records.sort_by_key(|(_, record)| record.id);
    Ok(records)
}

fn allocate_id(root: &Path) -> Result<u32> {
    let mut maximum = NEXT_ID_SEED.load(Ordering::Relaxed);
    for (_, record) in read_records(root)? {
        maximum = maximum.max(record.id);
    }
    let path = root.join("next-id");
    let current = fs::read_to_string(&path)
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
        .unwrap_or(0);
    let next = current
        .max(maximum)
        .checked_add(1)
        .ok_or_else(|| anyhow!("virtual display id space exhausted"))?;
    let temporary = root.join(format!("next-id-{}", std::process::id()));
    fs::write(&temporary, format!("{next}\n"))?;
    fs::rename(&temporary, &path)?;
    NEXT_ID_SEED.store(next, Ordering::Relaxed);
    Ok(next)
}

fn reservation_path(root: &Path, display: u32) -> PathBuf {
    root.join(format!(".display-{display}.lock"))
}

fn reserve_display(root: &Path, display: u32, directory_name: &str) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(O_NOFOLLOW)
        .open(reservation_path(root, display))?;
    file.write_all(directory_name.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

fn remove_reservation(root: &Path, display: u32, directory_name: &str) -> Result<()> {
    let path = reservation_path(root, display);
    match fs::read_to_string(&path) {
        Ok(owner) if owner == directory_name => {
            fs::remove_file(&path).context("remove X display reservation")?;
            Ok(())
        }
        Ok(owner) => {
            bail!("X display {display} reservation now belongs to {owner}, not {directory_name}")
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("read X display reservation"),
    }
}

fn x_lock_path(display: u32) -> PathBuf {
    // Xorg/Xvfb use the fixed system transport directory, not TMPDIR.
    PathBuf::from("/tmp").join(format!(".X{display}-lock"))
}

fn x_socket_path(display: u32) -> PathBuf {
    PathBuf::from("/tmp/.X11-unix").join(format!("X{display}"))
}

fn display_available(root: &Path, display: u32) -> Result<bool> {
    // Existing reservations include live records owned by other backend instances.
    if reservation_path(root, display).exists()
        || x_lock_path(display).exists()
        || x_socket_path(display).exists()
    {
        return Ok(false);
    }
    // Recheck after creating the reservation in spawn_server: Xvfb itself also uses the
    // standard X lock, so a foreign Xvfb racing us will cause a verified startup failure.
    Ok(true)
}

fn x_artifacts_owned(record: &Record) -> bool {
    let Ok(lock) = fs::read_to_string(x_lock_path(record.display)) else {
        return false;
    };
    let Some(pid) = lock.trim().parse::<u32>().ok() else {
        return false;
    };
    pid == record.pid
        && process_start_ticks(pid).ok().flatten() == Some(record.start_ticks)
        && x_socket_path(record.display).exists()
}

fn remove_x_artifacts(record: &Record) -> Result<()> {
    let lock_path = x_lock_path(record.display);
    match fs::read_to_string(&lock_path) {
        Ok(lock) => {
            let Ok(pid) = lock.trim().parse::<u32>() else {
                return Ok(());
            };
            // Only remove the standard artifacts if the lock still names our exact
            // process incarnation. A foreign/reused lock is never touched.
            if pid != record.pid
                || process_start_ticks(pid).ok().flatten() != Some(record.start_ticks)
                || process_running(pid, record.start_ticks)
            {
                return Ok(());
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("read Xvfb lock during cleanup"),
    }
    for path in [lock_path, x_socket_path(record.display)] {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("remove stale Xvfb artifact"),
        }
    }
    Ok(())
}

fn cleanup_stale_locked(
    root: &Path,
    current_instance: &str,
    state: &mut BackendState,
) -> Result<()> {
    // Orphan reservations from a crash between mkdir and spawn can be safely removed
    // while holding the cross-process registry lock, but never a live record's lock.
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(number) = name
            .strip_prefix(".display-")
            .and_then(|name| name.strip_suffix(".lock"))
        else {
            continue;
        };
        let Ok(_) = number.parse::<u32>() else {
            continue;
        };
        if let Ok(owner) = fs::read_to_string(&path) {
            if !owner.starts_with("display-") || !owner.bytes().all(|byte| byte.is_ascii_digit()) {
                continue; // never interpret a corrupt reservation as a path
            }
            let directory = root.join(&owner);
            if !directory.exists()
                || (read_record(&directory)?.is_none()
                    && fs::read_dir(&directory)?.next().is_none())
            {
                fs::remove_file(&path)?;
            }
        }
    }

    for (directory, record) in read_records(root)? {
        reap_child(state, &record);
        if record.is_owned_server() {
            if !instance_alive(&record.instance) {
                // Adopt a live orphan so a later failure is recreated by this instance,
                // rather than discarded as stale during the next cleanup.
                let mut adopted = record.clone();
                adopted.instance = current_instance.to_string();
                write_record(&directory.join("record"), &adopted)?;
            }
            continue;
        }
        // A live owner retains its desired spec for list/update health recreation.
        if should_remove_stale(false, instance_alive(&record.instance)) {
            remove_x_artifacts(&record)?;
            let name = directory
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            remove_reservation(root, record.display, &name)?;
            fs::remove_dir_all(&directory)?;
        }
    }

    // Remove empty instance directories only after all their records have been handled.
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name == current_instance
            || !valid_instance_name(name)
            || !fs::symlink_metadata(&path)?.is_dir()
        {
            continue;
        }
        let alive = instance_alive(name);
        if !alive {
            let _ = fs::remove_dir(&path);
        }
    }
    Ok(())
}

fn instance_alive(name: &str) -> bool {
    name.strip_prefix("instance-")
        .and_then(|rest| rest.split_once('-'))
        .and_then(|(pid, _)| pid.parse::<u32>().ok())
        .and_then(|pid| process_start_ticks(pid).ok().flatten())
        .is_some()
}

fn should_remove_stale(server_alive: bool, owner_alive: bool) -> bool {
    !server_alive && !owner_alive
}

/// Pure decision used by status/recovery: a matching handshake keeps a server, anything
/// else recreates it. Kept separate so state transitions are unit-testable without Xvfb.
fn needs_recreation(alive: bool, measured: Option<(u32, u32)>, expected: (u32, u32)) -> bool {
    !alive || measured != Some(expected)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> VirtualDisplaySpec {
        VirtualDisplaySpec::new(320, 240, 60, 96, "test desktop = 1")
    }

    #[test]
    fn xvfb_command_is_isolated_and_uses_custom_geometry() {
        let spec = VirtualDisplaySpec::new(642, 482, 75, 120, "ignored");
        let command = xvfb_command(123, &spec);
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                ":123",
                "-nolisten",
                "tcp",
                "-noreset",
                "-screen",
                "0",
                "642x482x24",
                "-dpi",
                "120"
            ]
        );
        assert!(command
            .get_envs()
            .any(|(key, value)| key == "DISPLAY"
                && value == Some(OsString::from(":123").as_os_str())));
    }

    #[test]
    fn query_commands_always_override_the_managed_display() {
        for command in [xdpyinfo_command(117), xrandr_fb_command(117, 800, 600)] {
            assert!(command.get_envs().any(|(key, value)| key == "DISPLAY"
                && value == Some(OsString::from(":117").as_os_str())));
        }
        let args: Vec<String> = xrandr_fb_command(117, 800, 600)
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, ["--display", ":117", "--fb", "800x600"]);
    }

    #[test]
    fn registry_record_round_trips_names_with_spaces_and_equals() {
        let record = Record {
            id: 7,
            display: 107,
            spec: spec(),
            pid: 1234,
            start_ticks: 98765,
            instance: "instance-123-abc".into(),
        };
        assert_eq!(Record::parse(&record.encode()).unwrap(), record);
    }

    #[test]
    fn registry_rejects_corrupt_or_noncanonical_records() {
        let record = Record {
            id: 7,
            display: 107,
            spec: spec(),
            pid: 1234,
            start_ticks: 98765,
            instance: "instance-123-abc".into(),
        };
        assert!(Record::parse(&record.encode().replace("format=1", "format=2")).is_err());
        assert!(Record::parse(&record.encode().replace("id=7", "id=0")).is_err());
        assert!(Record::parse(&record.encode().replace("width=320", "width=321")).is_err());
        assert!(Record::parse(
            &record
                .encode()
                .replace("instance=instance-123-abc", "instance=../other")
        )
        .is_err());
        assert!(Record::parse("format=1\nformat=1\n").is_err());
    }

    #[test]
    fn proc_start_time_parser_handles_spaces_and_parentheses_in_comm() {
        let stat =
            "123 (Xvfb (test) name) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 456789 0 0";
        assert_eq!(parse_proc_start_ticks(stat), Some(456789));
        assert_eq!(proc_state(stat), 'S');
        assert_eq!(parse_proc_start_ticks("bad"), None);
    }

    #[test]
    fn xdpyinfo_geometry_is_measured_not_inferred_from_exit_status() {
        let text = "name of display: :107\nversion number: 11.0\ndimensions: 800x600 pixels (423x317 millimeters)\n";
        assert_eq!(parse_xdpyinfo_dimensions(text), Some((800, 600)));
        assert_eq!(
            parse_xdpyinfo_dimensions("dimensions: 800x600 pixels"),
            Some((800, 600))
        );
        assert_eq!(parse_xdpyinfo_dimensions("dimensions: broken"), None);
        assert_eq!(parse_xdpyinfo_dimensions("number of screens: 1"), None);
    }

    #[test]
    fn recovery_state_requires_liveness_and_exact_geometry() {
        assert!(!needs_recreation(true, Some((320, 240)), (320, 240)));
        assert!(needs_recreation(false, Some((320, 240)), (320, 240)));
        assert!(needs_recreation(true, None, (320, 240)));
        assert!(needs_recreation(true, Some((321, 240)), (320, 240)));
    }

    #[test]
    fn stale_cleanup_keeps_desired_state_until_health_recreation() {
        assert!(
            !should_remove_stale(true, false),
            "live orphans are adopted"
        );
        assert!(
            !should_remove_stale(false, true),
            "dead servers with a live owner are recreated"
        );
        assert!(
            should_remove_stale(false, false),
            "only fully stale records are deleted"
        );
    }

    #[test]
    fn display_candidates_stay_in_the_private_range() {
        for seed in [0, 1, 899, 900, u32::MAX] {
            let first = FIRST_DISPLAY + seed % DISPLAY_COUNT;
            assert!((FIRST_DISPLAY..FIRST_DISPLAY + DISPLAY_COUNT).contains(&first));
        }
    }

    #[test]
    fn backend_rejects_invalid_specs_before_starting_a_process() {
        let backend = XvfbBackend::new().unwrap();
        let mut invalid = spec();
        invalid.height = 241;
        let error = backend.create(&invalid).unwrap_err();
        assert!(error.to_string().contains("odd"), "{error}");
        assert_eq!(
            backend
                .list()
                .unwrap()
                .iter()
                .filter(|info| info.spec.name_hint == invalid.name_hint)
                .count(),
            0
        );
    }

    #[test]
    fn unknown_ids_and_unsupported_mode_changes_are_errors() {
        let backend = XvfbBackend::new().unwrap();
        let spec = spec();
        assert!(backend
            .update(u32::MAX, &spec)
            .unwrap_err()
            .to_string()
            .contains("unknown virtual display id"));
        assert!(backend
            .destroy(u32::MAX)
            .unwrap_err()
            .to_string()
            .contains("unknown virtual display id"));
        assert!(backend.server_pid(u32::MAX).is_err());
        assert!(backend.display_name(u32::MAX).is_err());
    }

    #[test]
    fn reservation_files_are_exclusive() {
        let root = std::env::temp_dir().join(format!(
            "dfy-vdisplay-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        create_private_dir(&root).unwrap();
        reserve_display(&root, 123, "display-1").unwrap();
        assert!(reserve_display(&root, 123, "display-2").is_err());
        assert!(remove_reservation(&root, 123, "display-2").is_err());
        remove_reservation(&root, 123, "display-1").unwrap();
        reserve_display(&root, 123, "display-2").unwrap();
        remove_reservation(&root, 123, "display-2").unwrap();
        fs::remove_dir(&root).unwrap();
    }
}
