// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
//! Native X11 CLIPBOARD selections. No shell utilities, clipboard managers, or
//! network transport are started here. XFixes events invalidate snapshots;
//! only explicit `list_formats` / `read_block` calls request remote properties.
//!
//! ICCCM ownership is maintained by the service thread for the backend's
//! lifetime. Dropping the backend releases it; persisting after process exit
//! would require an explicitly integrated clipboard manager (SAVE_TARGETS).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xfixes::{ConnectionExt as _, SelectionEventMask};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, CreateWindowAux, EventMask,
    GetPropertyReply, PropMode, Property, SelectionNotifyEvent, SelectionRequestEvent, Window,
    WindowClass, SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{COPY_DEPTH_FROM_PARENT, CURRENT_TIME, NONE};

use crate::x11_state::{
    self, Encoding, Incoming, Outgoing, HARD_MAX_BYTES, MAX_TARGETS, MAX_TRANSFERS, TRANSFER_CHUNK,
};
use crate::{ClipFormat, ClipKind, ClipboardBackend, DataBlock, FormatList};

const TICK: Duration = Duration::from_millis(8);
const MAX_EVENTS_PER_TICK: usize = 256;

/// Native-backend memory ceiling. The synchronization protocol separately
/// retains its stricter [`crate::MAX_SYNC_BYTES`] limit (1 MiB).
pub const DEFAULT_X11_MAX_BYTES: usize = 16 * 1024 * 1024;

/// Limits are applied before copying outgoing data and at every received INCR
/// chunk. They do not grant session or network clipboard permissions.
#[derive(Debug, Clone)]
pub struct X11ClipboardOptions {
    pub max_bytes: usize,
    pub transfer_timeout: Duration,
}

impl Default for X11ClipboardOptions {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_X11_MAX_BYTES,
            transfer_timeout: Duration::from_secs(5),
        }
    }
}

impl X11ClipboardOptions {
    fn validate(&self) -> Result<()> {
        ensure!(
            (1..=HARD_MAX_BYTES).contains(&self.max_bytes),
            "X11 clipboard byte limit must be 1..={HARD_MAX_BYTES}"
        );
        ensure!(
            (Duration::from_millis(10)..=Duration::from_secs(30)).contains(&self.transfer_timeout),
            "X11 clipboard timeout must be between 10ms and 30s"
        );
        Ok(())
    }
}

/// A native X11 backend with a private unmapped owner window and service
/// thread. Its sequence is local to this instance, monotonically increases on
/// ownership events (including same-owner replacement), and is not an X atom
/// or a wrapping X server timestamp.
pub struct X11Clipboard {
    commands: SyncSender<Command>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    options: X11ClipboardOptions,
}

impl X11Clipboard {
    /// Open the current X11 desktop. Native Wayland is deliberately rejected:
    /// XWayland's selection covers X11 clients only, and does not establish
    /// compositor permission to read other Wayland applications.
    pub fn new(options: X11ClipboardOptions) -> Result<Self> {
        let session = std::env::var("XDG_SESSION_TYPE").ok();
        let wayland = std::env::var("WAYLAND_DISPLAY").ok();
        ensure_x11_session(session.as_deref(), wayland.as_deref())?;
        let display = std::env::var("DISPLAY").context("X11 clipboard requires DISPLAY and access to the desktop's Xauthority; native Wayland clipboard access requires compositor authorization")?;
        Self::connect_to_display(&display, options)
    }

    /// Explicitly select an X server (also useful for isolated Xvfb tests).
    /// On Wayland this method is an explicit XWayland-only operation; it does
    /// not provide native Wayland/global desktop clipboard access.
    pub fn connect_to_display(display: &str, options: X11ClipboardOptions) -> Result<Self> {
        options.validate()?;
        ensure!(
            !display.trim().is_empty(),
            "X11 clipboard display must not be empty"
        );
        let stop = Arc::new(AtomicBool::new(false));
        let worker = Worker::connect(display, options.clone(), Arc::clone(&stop))?;
        let (commands, incoming) = mpsc::sync_channel(8);
        let join = thread::Builder::new()
            .name("dfy-x11-clipboard".into())
            .spawn(move || {
                if let Err(err) = worker.run(incoming) {
                    tracing::warn!(error = %err, "X11 clipboard service stopped");
                }
            })
            .context("Start X11 clipboard service")?;
        Ok(Self {
            commands,
            stop,
            join: Some(join),
            options,
        })
    }

    /// Subscribe to ownership-change wakeups from this backend. No selection
    /// payloads are read. Notifications can coalesce; query `sequence()` after
    /// waking. Sequence values are comparable only within this backend.
    pub fn subscribe_changes(&self) -> Result<Receiver<u64>> {
        match self.call(Operation::Subscribe)? {
            Reply::Subscription(receiver) => Ok(receiver),
            _ => bail!("Invalid X11 clipboard subscription reply"),
        }
    }

    fn call(&self, operation: Operation) -> Result<Reply> {
        let (reply, receive) = mpsc::sync_channel(1);
        self.commands
            .send(Command { operation, reply })
            .context("X11 clipboard service is no longer available")?;
        receive
            .recv_timeout(self.options.transfer_timeout + Duration::from_secs(2))
            .context("Timed out waiting for X11 clipboard service")?
    }
}

impl Drop for X11Clipboard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl ClipboardBackend for X11Clipboard {
    fn sequence(&self) -> Result<u64> {
        match self.call(Operation::Sequence)? {
            Reply::Sequence(sequence) => Ok(sequence),
            _ => bail!("Invalid X11 clipboard sequence reply"),
        }
    }

    fn list_formats(&self) -> Result<FormatList> {
        match self.call(Operation::List)? {
            Reply::Formats(list) => Ok(list),
            _ => bail!("Invalid X11 clipboard format reply"),
        }
    }

    fn read_block(&self, format_id: u32, offset: u64, max: usize) -> Result<DataBlock> {
        match self.call(Operation::Read {
            format_id,
            offset,
            max,
        })? {
            Reply::Block(block) => Ok(block),
            _ => bail!("Invalid X11 clipboard read reply"),
        }
    }

    fn write(&mut self, kind: &ClipKind, bytes: &[u8], files: &[PathBuf]) -> Result<()> {
        ensure!(
            files.is_empty(),
            "X11 clipboard file references are not supported"
        );
        ensure!(
            bytes.len() <= self.options.max_bytes,
            "X11 clipboard payload exceeds byte limit ({})",
            self.options.max_bytes
        );
        ensure!(
            matches!(kind, ClipKind::Text | ClipKind::Html | ClipKind::Png),
            "X11 clipboard supports Unicode text, HTML and PNG; unsupported kind: {kind}"
        );
        if matches!(kind, ClipKind::Text | ClipKind::Html) {
            std::str::from_utf8(bytes).context("X11 clipboard text/HTML must be UTF-8")?;
        }
        match self.call(Operation::Write {
            kind: kind.clone(),
            bytes: Arc::from(bytes),
        })? {
            Reply::Written => Ok(()),
            _ => bail!("Invalid X11 clipboard write reply"),
        }
    }
}

pub(crate) fn create_backend() -> Result<Box<dyn ClipboardBackend>> {
    Ok(Box::new(X11Clipboard::new(X11ClipboardOptions::default())?))
}

fn ensure_x11_session(session: Option<&str>, wayland: Option<&str>) -> Result<()> {
    if session.is_some_and(|s| s.eq_ignore_ascii_case("wayland"))
        || wayland.is_some_and(|s| !s.is_empty())
    {
        bail!("Native Wayland clipboard is unavailable: compositor authorization/data-control or an appropriate portal is required; DISPLAY/XWayland does not grant global clipboard permission. Use an X11 session, or explicitly open an X11-only display with X11Clipboard::connect_to_display");
    }
    Ok(())
}

struct Command {
    operation: Operation,
    reply: SyncSender<Result<Reply>>,
}

enum Operation {
    Sequence,
    List,
    Read {
        format_id: u32,
        offset: u64,
        max: usize,
    },
    Write {
        kind: ClipKind,
        bytes: Arc<[u8]>,
    },
    Subscribe,
}

enum Reply {
    Sequence(u64),
    Formats(FormatList),
    Block(DataBlock),
    Written,
    Subscription(Receiver<u64>),
}

struct Atoms {
    clipboard: Atom,
    targets: Atom,
    timestamp: Atom,
    incr: Atom,
    utf8: Atom,
    text_utf8: Atom,
    text_plain: Atom,
    html: Atom,
    html_utf8: Atom,
    png: Atom,
    transfer: Atom,
    clock: Atom,
}

impl Atoms {
    fn new(conn: &RustConnection) -> Result<Self> {
        let intern =
            |name: &[u8]| -> Result<Atom> { Ok(conn.intern_atom(false, name)?.reply()?.atom) };
        Ok(Self {
            clipboard: intern(b"CLIPBOARD")?,
            targets: intern(b"TARGETS")?,
            timestamp: intern(b"TIMESTAMP")?,
            incr: intern(b"INCR")?,
            utf8: intern(b"UTF8_STRING")?,
            text_utf8: intern(b"text/plain;charset=utf-8")?,
            text_plain: intern(b"text/plain")?,
            html: intern(b"text/html")?,
            html_utf8: intern(b"text/html;charset=utf-8")?,
            png: intern(b"image/png")?,
            transfer: intern(b"_DFY_CLIPBOARD_TRANSFER")?,
            clock: intern(b"_DFY_CLIPBOARD_TIME")?,
        })
    }

    fn formats(&self, available: &[Atom]) -> Vec<FormatSpec> {
        let mut formats = Vec::new();
        for (targets, kind) in [
            (
                vec![
                    (self.utf8, Encoding::Utf8),
                    (self.text_utf8, Encoding::Utf8),
                    (self.text_plain, Encoding::Utf8),
                    (u32::from(AtomEnum::STRING), Encoding::Latin1),
                ],
                ClipKind::Text,
            ),
            (
                vec![
                    (self.html_utf8, Encoding::Html),
                    (self.html, Encoding::Html),
                ],
                ClipKind::Html,
            ),
            (vec![(self.png, Encoding::Png)], ClipKind::Png),
        ] {
            if let Some((atom, encoding)) = targets
                .into_iter()
                .find(|(atom, _)| available.contains(atom))
            {
                formats.push(FormatSpec {
                    atom,
                    encoding,
                    kind,
                });
            }
        }
        formats
    }
}

#[derive(Clone)]
struct FormatSpec {
    atom: Atom,
    encoding: Encoding,
    kind: ClipKind,
}

impl FormatSpec {
    fn descriptor(&self) -> ClipFormat {
        // Canonical protocol names retain compatibility with ClipboardSync.
        let name = match self.kind {
            ClipKind::Text => "CF_UNICODETEXT",
            ClipKind::Html => "HTML Format",
            ClipKind::Png => "PNG",
            _ => unreachable!(),
        };
        ClipFormat {
            id: self.atom,
            name: name.into(),
            kind: self.kind.clone(),
        }
    }
}

struct Owned {
    kind: ClipKind,
    bytes: Arc<[u8]>,
    latin1: Option<Arc<[u8]>>,
    timestamp: u32,
}

impl Owned {
    fn targets(&self, atoms: &Atoms) -> Vec<Atom> {
        let mut targets = vec![atoms.targets, atoms.timestamp];
        match self.kind {
            ClipKind::Text => {
                targets.extend([atoms.utf8, atoms.text_utf8, atoms.text_plain]);
                if self.latin1.is_some() {
                    targets.push(AtomEnum::STRING.into());
                }
            }
            ClipKind::Html => targets.extend([atoms.html_utf8, atoms.html]),
            ClipKind::Png => targets.push(atoms.png),
            _ => {}
        }
        targets
    }

    fn payload(&self, atoms: &Atoms, target: Atom) -> Option<Arc<[u8]>> {
        if self.kind == ClipKind::Text && target == u32::from(AtomEnum::STRING) {
            return self.latin1.clone();
        }
        self.targets(atoms)
            .contains(&target)
            .then(|| Arc::clone(&self.bytes))
    }
}

struct SendTransfer {
    target: Atom,
    stream: Outgoing,
    deadline: Instant,
}

struct PropertyData {
    type_: Atom,
    format: u8,
    bytes: Vec<u8>,
}

struct Worker {
    conn: RustConnection,
    root: Window,
    window: Window,
    atoms: Atoms,
    owner: Window,
    sequence: u64,
    options: X11ClipboardOptions,
    stop: Arc<AtomicBool>,
    owned: Option<Owned>,
    pending_owned: Option<Owned>,
    formats: Option<Vec<FormatSpec>>,
    cache: Option<(Atom, Arc<[u8]>)>,
    outgoing: HashMap<(Window, Atom), SendTransfer>,
    subscribers: Vec<SyncSender<u64>>,
    chunk: usize,
}

impl Worker {
    fn connect(display: &str, options: X11ClipboardOptions, stop: Arc<AtomicBool>) -> Result<Self> {
        let (conn, screen) = x11rb::connect(Some(display))
            .context("Connect to X11 clipboard display (check DISPLAY/Xauthority)")?;
        let version = conn
            .xfixes_query_version(2, 0)
            .context("X11 clipboard requires XFixes 2.0 ownership notifications")?
            .reply()?;
        ensure!(
            version.major_version >= 2,
            "X11 clipboard requires XFixes 2.0 ownership notifications"
        );
        let root = conn.setup().roots[screen].root;
        let atoms = Atoms::new(&conn)?;
        let window = create_window(&conn, root)?;
        conn.xfixes_select_selection_input(
            window,
            atoms.clipboard,
            SelectionEventMask::SET_SELECTION_OWNER
                | SelectionEventMask::SELECTION_WINDOW_DESTROY
                | SelectionEventMask::SELECTION_CLIENT_CLOSE,
        )?
        .check()?;
        let owner = conn.get_selection_owner(atoms.clipboard)?.reply()?.owner;
        let chunk = conn
            .maximum_request_bytes()
            .saturating_sub(64)
            .clamp(1, TRANSFER_CHUNK);
        conn.flush()?;
        Ok(Self {
            conn,
            root,
            window,
            atoms,
            owner,
            sequence: 1,
            options,
            stop,
            owned: None,
            pending_owned: None,
            formats: None,
            cache: None,
            outgoing: HashMap::new(),
            subscribers: Vec::new(),
            chunk,
        })
    }

    fn run(mut self, commands: Receiver<Command>) -> Result<()> {
        while !self.stop.load(Ordering::Acquire) {
            self.drain_events()?;
            self.expire_transfers();
            match commands.recv_timeout(TICK) {
                Ok(command) => {
                    let result = self
                        .refresh_owner()
                        .and_then(|()| self.execute(command.operation));
                    let _ = command.reply.send(result);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        // Disconnecting destroys our unmapped owner window and aborts pending
        // transfers. No clipboard payload is handed to an external manager.
        Ok(())
    }

    fn execute(&mut self, operation: Operation) -> Result<Reply> {
        match operation {
            Operation::Sequence => Ok(Reply::Sequence(self.sequence)),
            Operation::List => Ok(Reply::Formats(self.list_formats()?)),
            Operation::Read {
                format_id,
                offset,
                max,
            } => Ok(Reply::Block(self.read_block(format_id, offset, max)?)),
            Operation::Write { kind, bytes } => {
                self.write(kind, bytes)?;
                Ok(Reply::Written)
            }
            Operation::Subscribe => {
                ensure!(
                    self.subscribers.len() < 16,
                    "Too many X11 clipboard subscribers"
                );
                let (send, receive) = mpsc::sync_channel(1);
                self.subscribers.push(send);
                Ok(Reply::Subscription(receive))
            }
        }
    }

    fn refresh_owner(&mut self) -> Result<()> {
        // This roundtrip places earlier XFixes events in our event queue, even
        // when the caller races another client's ownership request.
        let _owner = self
            .conn
            .get_selection_owner(self.atoms.clipboard)?
            .reply()?
            .owner;
        ensure!(
            self.drain_events()?,
            "X11 clipboard event backlog; retry the snapshot"
        );
        Ok(())
    }

    fn ownership_changed(&mut self, owner: Window) -> Result<()> {
        self.sequence = self
            .sequence
            .checked_add(1)
            .context("X11 clipboard sequence exhausted")?;
        self.owner = owner;
        self.formats = None;
        self.cache = None;
        if owner != self.window {
            self.owned = None;
        }
        // Already accepted INCR transfers keep their Arc snapshot after losing
        // ownership, as required by ICCCM. New requests are refused.
        self.subscribers.retain(|send| {
            !matches!(
                send.try_send(self.sequence),
                Err(TrySendError::Disconnected(_))
            )
        });
        Ok(())
    }

    fn drain_events(&mut self) -> Result<bool> {
        // A misbehaving X client cannot starve command handling indefinitely.
        for _ in 0..MAX_EVENTS_PER_TICK {
            let Some(event) = self.conn.poll_for_event()? else {
                return Ok(true);
            };
            self.handle_event(event)?;
        }
        Ok(false)
    }

    fn handle_event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::XfixesSelectionNotify(event)
                if event.window == self.window && event.selection == self.atoms.clipboard =>
            {
                self.ownership_changed(event.owner)?;
                if event.owner == self.window
                    && self
                        .pending_owned
                        .as_ref()
                        .is_some_and(|owned| owned.timestamp == event.selection_timestamp)
                {
                    self.owned = self.pending_owned.take();
                }
            }
            // XFixes also covers owner destruction and client disconnect. A
            // separate SelectionClear may be queued before reacquisition in
            // the same server millisecond, so it must not clear the new data.
            Event::SelectionClear(_) => {}
            Event::SelectionRequest(event) => {
                if let Err(err) = self.serve(event) {
                    // Requestor windows can disappear at any point. This must
                    // not kill the owner thread or expose clipboard contents.
                    tracing::debug!(error = %err, "X11 clipboard request refused");
                    let _ = self.notify(event, NONE);
                }
            }
            Event::PropertyNotify(event) if event.state == Property::DELETE => {
                self.send_next(event.window, event.atom)
            }
            Event::DestroyNotify(event) => self
                .outgoing
                .retain(|(window, _), _| *window != event.window),
            Event::Error(error) => {
                tracing::debug!(error = ?error, "Asynchronous X11 clipboard error")
            }
            _ => {}
        }
        Ok(())
    }

    fn list_formats(&mut self) -> Result<FormatList> {
        if self.formats.is_none() {
            let formats = if self.owner == NONE {
                Vec::new()
            } else if let Some(owned) = &self.owned {
                self.atoms.formats(&owned.targets(&self.atoms))
            } else {
                let sequence = self.sequence;
                let data = self.convert(self.atoms.targets, MAX_TARGETS * 4)?;
                ensure!(
                    sequence == self.sequence,
                    "X11 clipboard ownership changed while listing formats"
                );
                ensure!(
                    data.type_ == u32::from(AtomEnum::ATOM)
                        && data.format == 32
                        && data.bytes.len() % 4 == 0,
                    "Invalid X11 clipboard TARGETS property"
                );
                // x11rb's wire byte order is native endian, including property
                // values returned by GetProperty.
                let targets = data
                    .bytes
                    .chunks_exact(4)
                    .map(|part| u32::from_ne_bytes(part.try_into().expect("four-byte atom")))
                    .collect::<Vec<_>>();
                self.atoms.formats(&targets)
            };
            self.formats = Some(formats);
        }
        Ok(FormatList {
            sequence: self.sequence,
            formats: self
                .formats
                .as_ref()
                .expect("formats initialized")
                .iter()
                .map(FormatSpec::descriptor)
                .collect(),
        })
    }

    fn read_block(&mut self, format_id: Atom, offset: u64, max: usize) -> Result<DataBlock> {
        // An atom from an obsolete list must not cause an unannounced read of
        // the replacement selection. The caller must enumerate it first.
        let spec = self
            .formats
            .as_ref()
            .and_then(|formats| formats.iter().find(|format| format.atom == format_id))
            .cloned()
            .context(
                "X11 clipboard format snapshot is missing or stale; call list_formats first",
            )?;
        if self
            .cache
            .as_ref()
            .map_or(true, |(atom, _)| *atom != format_id)
        {
            let sequence = self.sequence;
            let bytes = if let Some(owned) = &self.owned {
                owned
                    .payload(&self.atoms, format_id)
                    .context("X11 clipboard target is no longer available")?
                    .to_vec()
            } else {
                let data = self.convert(format_id, self.options.max_bytes)?;
                ensure!(
                    data.format == 8,
                    "X11 clipboard payload is not an 8-bit property"
                );
                let valid_type = match spec.encoding {
                    Encoding::Utf8 => {
                        [self.atoms.utf8, self.atoms.text_utf8, self.atoms.text_plain]
                            .contains(&data.type_)
                    }
                    Encoding::Latin1 => data.type_ == u32::from(AtomEnum::STRING),
                    Encoding::Html => [self.atoms.html, self.atoms.html_utf8].contains(&data.type_),
                    Encoding::Png => data.type_ == self.atoms.png,
                };
                ensure!(
                    valid_type,
                    "X11 clipboard returned an unexpected property type"
                );
                data.bytes
            };
            ensure!(
                sequence == self.sequence,
                "X11 clipboard ownership changed during payload read"
            );
            let decoded = x11_state::decode(spec.encoding, bytes, self.options.max_bytes)?;
            self.cache = Some((format_id, Arc::from(decoded)));
        }
        let (_, bytes) = self.cache.as_ref().expect("cache initialized");
        Ok(x11_state::block(format_id, bytes, offset, max))
    }

    fn write(&mut self, kind: ClipKind, bytes: Arc<[u8]>) -> Result<()> {
        let timestamp = self.server_time()?;
        let latin1 = if kind == ClipKind::Text {
            x11_state::latin1(&bytes)
        } else {
            None
        };
        self.pending_owned = Some(Owned {
            kind,
            bytes,
            latin1,
            timestamp,
        });
        let result = (|| -> Result<()> {
            self.conn
                .set_selection_owner(self.window, self.atoms.clipboard, timestamp)?
                .check()?;
            let owner = self
                .conn
                .get_selection_owner(self.atoms.clipboard)?
                .reply()?
                .owner;
            ensure!(
                owner == self.window,
                "X11 clipboard ownership was refused or immediately replaced"
            );
            let deadline = Instant::now() + self.options.transfer_timeout;
            while self.pending_owned.is_some() {
                let event = self.next_event(deadline)?;
                self.handle_event(event)?;
            }
            self.refresh_owner()?;
            ensure!(
                self.owner == self.window && self.owned.is_some(),
                "X11 clipboard ownership was immediately replaced"
            );
            Ok(())
        })();
        self.pending_owned = None;
        result
    }

    fn server_time(&mut self) -> Result<u32> {
        self.conn.change_property8(
            PropMode::REPLACE,
            self.window,
            self.atoms.clock,
            AtomEnum::INTEGER,
            &[0],
        )?;
        self.conn.flush()?;
        let deadline = Instant::now() + self.options.transfer_timeout;
        loop {
            let event = self.next_event(deadline)?;
            if let Event::PropertyNotify(event) = &event {
                if event.window == self.window
                    && event.atom == self.atoms.clock
                    && event.state == Property::NEW_VALUE
                {
                    return Ok(event.time);
                }
            }
            self.handle_event(event)?;
        }
    }

    fn next_event(&mut self, deadline: Instant) -> Result<Event> {
        loop {
            ensure!(
                !self.stop.load(Ordering::Acquire),
                "X11 clipboard service is stopping"
            );
            ensure!(
                Instant::now() < deadline,
                "X11 clipboard selection transfer timed out"
            );
            if let Some(event) = self.conn.poll_for_event()? {
                return Ok(event);
            }
            self.expire_transfers();
            thread::sleep(TICK.min(deadline.saturating_duration_since(Instant::now())));
        }
    }

    fn convert(&mut self, target: Atom, limit: usize) -> Result<PropertyData> {
        // A fresh requestor window prevents late replies from a timed-out
        // conversion being mistaken for a subsequent request of the same type.
        let requestor = create_window(&self.conn, self.root)?;
        let result = self.convert_on(requestor, target, limit);
        let _ = self.conn.destroy_window(requestor);
        let _ = self.conn.flush();
        result
    }

    fn convert_on(
        &mut self,
        requestor: Window,
        target: Atom,
        limit: usize,
    ) -> Result<PropertyData> {
        let sequence = self.sequence;
        let deadline = Instant::now() + self.options.transfer_timeout;
        self.conn
            .convert_selection(
                requestor,
                self.atoms.clipboard,
                target,
                self.atoms.transfer,
                CURRENT_TIME,
            )?
            .check()?;
        self.conn.flush()?;
        loop {
            let event = self.next_event(deadline)?;
            if let Event::SelectionNotify(event) = &event {
                if event.requestor == requestor
                    && event.selection == self.atoms.clipboard
                    && event.target == target
                {
                    ensure!(
                        event.property == self.atoms.transfer,
                        "X11 clipboard owner refused requested target"
                    );
                    break;
                }
            }
            self.handle_event(event)?;
            ensure!(
                sequence == self.sequence,
                "X11 clipboard ownership changed during selection conversion"
            );
        }
        let first = self.get_property(requestor, limit)?;
        if first.type_ != self.atoms.incr {
            return Ok(PropertyData {
                type_: first.type_,
                format: first.format,
                bytes: first.value,
            });
        }
        ensure!(
            first.format == 32 && first.value_len == 1,
            "Invalid X11 clipboard INCR header"
        );
        let advertised = first
            .value32()
            .and_then(|mut values| values.next())
            .context("Missing X11 clipboard INCR length")?;
        let mut incoming = Incoming::new(advertised, limit)?;
        // GetProperty(delete=true) removed the INCR header; that deletion is
        // the handshake acknowledgement that allows the owner to send data.
        self.conn.flush()?;
        loop {
            let event = self.next_event(deadline)?;
            if let Event::PropertyNotify(event) = &event {
                if event.window == requestor
                    && event.atom == self.atoms.transfer
                    && event.state == Property::NEW_VALUE
                {
                    let part = self.get_property(requestor, limit)?;
                    let finished = incoming.push(part.type_, part.format, &part.value)?;
                    self.conn.flush()?;
                    if finished {
                        let (type_, format, bytes) = incoming.finish()?;
                        return Ok(PropertyData {
                            type_,
                            format,
                            bytes,
                        });
                    }
                    continue;
                }
            }
            self.handle_event(event)?;
            ensure!(
                sequence == self.sequence,
                "X11 clipboard ownership changed during INCR transfer"
            );
        }
    }

    fn get_property(&self, window: Window, limit: usize) -> Result<GetPropertyReply> {
        let units =
            u32::try_from(limit.saturating_add(3) / 4).context("X11 property limit overflow")?;
        let property = self
            .conn
            .get_property(true, window, self.atoms.transfer, AtomEnum::ANY, 0, units)?
            .reply()?;
        ensure!(
            property.bytes_after == 0 && property.value.len() <= limit,
            "X11 clipboard property exceeds byte limit ({limit})"
        );
        ensure!(
            property.type_ != NONE,
            "X11 clipboard owner returned no property"
        );
        Ok(property)
    }

    fn notify(&self, request: SelectionRequestEvent, property: Atom) -> Result<()> {
        let event = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: request.time,
            requestor: request.requestor,
            selection: request.selection,
            target: request.target,
            property,
        };
        self.conn
            .send_event(false, request.requestor, EventMask::NO_EVENT, event)?
            .check()?;
        self.conn.flush()?;
        Ok(())
    }

    fn serve(&mut self, request: SelectionRequestEvent) -> Result<()> {
        ensure!(
            request.owner == self.window
                && request.selection == self.atoms.clipboard
                && self.owner == self.window,
            "Selection request is not for our CLIPBOARD ownership"
        );
        let owned = self
            .owned
            .as_ref()
            .context("X11 clipboard has no owned payload")?;
        ensure!(
            request.time == CURRENT_TIME
                || !x11_state::timestamp_before(request.time, owned.timestamp),
            "Selection request predates X11 clipboard ownership"
        );
        // ICCCM compatibility with requestors that use property=None.
        let property = if request.property == NONE {
            request.target
        } else {
            request.property
        };
        ensure!(property != NONE, "X11 clipboard request has no property");
        if request.target == self.atoms.targets {
            self.conn
                .change_property32(
                    PropMode::REPLACE,
                    request.requestor,
                    property,
                    AtomEnum::ATOM,
                    &owned.targets(&self.atoms),
                )?
                .check()?;
        } else if request.target == self.atoms.timestamp {
            self.conn
                .change_property32(
                    PropMode::REPLACE,
                    request.requestor,
                    property,
                    AtomEnum::INTEGER,
                    &[owned.timestamp],
                )?
                .check()?;
        } else {
            let bytes = owned
                .payload(&self.atoms, request.target)
                .context("Unsupported X11 clipboard target")?;
            let key = (request.requestor, property);
            ensure!(
                !self.outgoing.contains_key(&key),
                "X11 clipboard request overlaps an active INCR property"
            );
            if bytes.len() <= self.chunk {
                self.conn
                    .change_property8(
                        PropMode::REPLACE,
                        request.requestor,
                        property,
                        request.target,
                        &bytes,
                    )?
                    .check()?;
            } else {
                ensure!(
                    self.outgoing.len() < MAX_TRANSFERS,
                    "Too many concurrent X11 clipboard INCR transfers"
                );
                self.conn
                    .change_window_attributes(
                        request.requestor,
                        &ChangeWindowAttributesAux::new()
                            .event_mask(EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY),
                    )?
                    .check()?;
                self.conn
                    .change_property32(
                        PropMode::REPLACE,
                        request.requestor,
                        property,
                        self.atoms.incr,
                        &[bytes.len() as u32],
                    )?
                    .check()?;
                self.outgoing.insert(
                    key,
                    SendTransfer {
                        target: request.target,
                        stream: Outgoing::new(bytes, self.chunk)?,
                        deadline: Instant::now() + self.options.transfer_timeout,
                    },
                );
            }
        }
        self.notify(request, property)
    }

    fn send_next(&mut self, window: Window, property: Atom) {
        let key = (window, property);
        let Some(transfer) = self.outgoing.get_mut(&key) else {
            return;
        };
        let result = (|| -> Result<bool> {
            let bytes = transfer.stream.next()?;
            self.conn
                .change_property8(PropMode::REPLACE, window, property, transfer.target, bytes)?
                .check()?;
            self.conn.flush()?;
            Ok(bytes.is_empty())
        })();
        if !matches!(result, Ok(false)) {
            self.outgoing.remove(&key);
            self.unwatch_requestor(window);
        }
    }

    fn unwatch_requestor(&self, window: Window) {
        if window != self.window
            && !self
                .outgoing
                .keys()
                .any(|(requestor, _)| *requestor == window)
        {
            let _ = self.conn.change_window_attributes(
                window,
                &ChangeWindowAttributesAux::new().event_mask(EventMask::NO_EVENT),
            );
            let _ = self.conn.flush();
        }
    }

    fn expire_transfers(&mut self) {
        let expired = self
            .outgoing
            .iter()
            .filter_map(|(&(window, property), transfer)| {
                (Instant::now() >= transfer.deadline).then_some((window, property))
            })
            .collect::<Vec<_>>();
        for key in expired {
            self.outgoing.remove(&key);
            self.unwatch_requestor(key.0);
        }
    }
}

fn create_window(conn: &RustConnection, root: Window) -> Result<Window> {
    let window = conn.generate_id()?;
    conn.create_window(
        COPY_DEPTH_FROM_PARENT,
        window,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &CreateWindowAux::new()
            .event_mask(EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY),
    )?
    .check()?;
    Ok(window)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wayland_permission_does_not_follow_from_display() {
        assert!(ensure_x11_session(Some("wayland"), None).is_err());
        assert!(ensure_x11_session(None, Some("wayland-0")).is_err());
        assert!(ensure_x11_session(Some("x11"), None).is_ok());
    }

    #[test]
    fn options_reject_unbounded_memory_and_waits() {
        let mut options = X11ClipboardOptions::default();
        assert!(options.validate().is_ok());
        options.max_bytes = HARD_MAX_BYTES + 1;
        assert!(options.validate().is_err());
        options.max_bytes = 1;
        options.transfer_timeout = Duration::ZERO;
        assert!(options.validate().is_err());
    }

    #[derive(Clone, Copy)]
    enum ExternalResponse {
        Direct,
        Incr,
        Stall,
    }

    /// Independent protocol peer: does not use the backend or its conversion
    /// helper. A deliberately false INCR hint tests the real receive path.
    struct ExternalOwner {
        stop: Arc<AtomicBool>,
        join: Option<JoinHandle<()>>,
        requests: Arc<std::sync::Mutex<Vec<Atom>>>,
        targets: Atom,
        utf8: Atom,
    }

    impl ExternalOwner {
        fn start(display: &str, bytes: Vec<u8>, mode: ExternalResponse) -> Self {
            let (conn, screen) = x11rb::connect(Some(display)).unwrap();
            let atoms = Atoms::new(&conn).unwrap();
            let window = create_window(&conn, conn.setup().roots[screen].root).unwrap();
            conn.set_selection_owner(window, atoms.clipboard, CURRENT_TIME)
                .unwrap()
                .check()
                .unwrap();
            assert_eq!(
                conn.get_selection_owner(atoms.clipboard)
                    .unwrap()
                    .reply()
                    .unwrap()
                    .owner,
                window
            );
            let stop = Arc::new(AtomicBool::new(false));
            let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
            let (thread_stop, thread_requests) = (Arc::clone(&stop), Arc::clone(&requests));
            let (targets, utf8) = (atoms.targets, atoms.utf8);
            let join = thread::spawn(move || {
                let bytes: Arc<[u8]> = Arc::from(bytes);
                let mut transfers: HashMap<(Window, Atom), Outgoing> = HashMap::new();
                while !thread_stop.load(Ordering::Acquire) {
                    let Some(event) = conn.poll_for_event().unwrap() else {
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    };
                    match event {
                        Event::SelectionRequest(request) => {
                            thread_requests.lock().unwrap().push(request.target);
                            let property = if request.property == NONE {
                                request.target
                            } else {
                                request.property
                            };
                            if request.target == atoms.targets {
                                conn.change_property32(
                                    PropMode::REPLACE,
                                    request.requestor,
                                    property,
                                    AtomEnum::ATOM,
                                    &[atoms.utf8],
                                )
                                .unwrap()
                                .check()
                                .unwrap();
                            } else if request.target == atoms.utf8 {
                                match mode {
                                    ExternalResponse::Stall => continue,
                                    ExternalResponse::Direct => {
                                        conn.change_property8(
                                            PropMode::REPLACE,
                                            request.requestor,
                                            property,
                                            atoms.utf8,
                                            &bytes,
                                        )
                                        .unwrap()
                                        .check()
                                        .unwrap();
                                    }
                                    ExternalResponse::Incr => {
                                        conn.change_window_attributes(
                                            request.requestor,
                                            &ChangeWindowAttributesAux::new().event_mask(
                                                EventMask::PROPERTY_CHANGE
                                                    | EventMask::STRUCTURE_NOTIFY,
                                            ),
                                        )
                                        .unwrap()
                                        .check()
                                        .unwrap();
                                        conn.change_property32(
                                            PropMode::REPLACE,
                                            request.requestor,
                                            property,
                                            atoms.incr,
                                            &[1],
                                        )
                                        .unwrap()
                                        .check()
                                        .unwrap();
                                        transfers.insert(
                                            (request.requestor, property),
                                            Outgoing::new(Arc::clone(&bytes), 31).unwrap(),
                                        );
                                    }
                                }
                            } else {
                                continue;
                            }
                            let notify = SelectionNotifyEvent {
                                response_type: SELECTION_NOTIFY_EVENT,
                                sequence: 0,
                                time: request.time,
                                requestor: request.requestor,
                                selection: request.selection,
                                target: request.target,
                                property,
                            };
                            conn.send_event(false, request.requestor, EventMask::NO_EVENT, notify)
                                .unwrap()
                                .check()
                                .unwrap();
                            conn.flush().unwrap();
                        }
                        Event::PropertyNotify(event) if event.state == Property::DELETE => {
                            let key = (event.window, event.atom);
                            if let Some(stream) = transfers.get_mut(&key) {
                                let chunk = stream.next().unwrap();
                                let result = conn
                                    .change_property8(
                                        PropMode::REPLACE,
                                        event.window,
                                        event.atom,
                                        atoms.utf8,
                                        chunk,
                                    )
                                    .unwrap()
                                    .check();
                                if chunk.is_empty() || result.is_err() {
                                    transfers.remove(&key);
                                }
                                conn.flush().unwrap();
                            }
                        }
                        Event::DestroyNotify(event) => {
                            transfers.retain(|(window, _), _| *window != event.window)
                        }
                        _ => {}
                    }
                }
            });
            Self {
                stop,
                join: Some(join),
                requests,
                targets,
                utf8,
            }
        }
    }

    impl Drop for ExternalOwner {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(join) = self.join.take() {
                join.join().unwrap();
            }
        }
    }

    #[test]
    #[ignore = "Requires an explicitly isolated Xvfb server in DFY_CLIPBOARD_TEST_DISPLAY; never uses DISPLAY"]
    fn isolated_x11_roundtrip_and_owner_lifetime() {
        let display = std::env::var("DFY_CLIPBOARD_TEST_DISPLAY")
            .expect("Start a private Xvfb and set DFY_CLIPBOARD_TEST_DISPLAY explicitly");
        let mut owner =
            X11Clipboard::connect_to_display(&display, X11ClipboardOptions::default()).unwrap();
        let reader =
            X11Clipboard::connect_to_display(&display, X11ClipboardOptions::default()).unwrap();
        let changes = reader.subscribe_changes().unwrap();
        let before = reader.sequence().unwrap();
        let text = "大肥鱼 🐟\n".repeat(20_000).into_bytes();
        owner.write(&ClipKind::Text, &text, &[]).unwrap();
        changes.recv_timeout(Duration::from_secs(2)).unwrap();
        let listed = reader.list_formats().unwrap();
        assert!(listed.sequence > before);
        assert_eq!(listed.formats.len(), 1);
        assert_eq!(listed.formats[0].name, "CF_UNICODETEXT");
        let id = listed.formats[0].id;
        let probe = reader.read_block(id, 0, 0).unwrap();
        assert_eq!(probe.total_len, text.len() as u64);
        assert!(probe.block.is_empty());
        let mut received = Vec::new();
        while received.len() < text.len() {
            received.extend(
                reader
                    .read_block(id, received.len() as u64, 7919)
                    .unwrap()
                    .block,
            );
        }
        assert_eq!(received, text);
        let mut png_bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png_bytes, 1, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[20, 40, 200, 255]).unwrap();
            writer.finish().unwrap();
        }
        for (kind, bytes) in [
            (ClipKind::Html, "<b>大肥鱼</b>".as_bytes()),
            (ClipKind::Png, png_bytes.as_slice()),
        ] {
            owner.write(&kind, bytes, &[]).unwrap();
            // Sequence roundtrip flushes pending XFixes events and invalidates
            // the earlier snapshot before the next enumeration.
            let _ = reader.sequence().unwrap();
            assert!(reader.read_block(id, 0, 1).is_err());
            let list = reader.list_formats().unwrap();
            assert_eq!(list.formats[0].kind, kind);
            assert_eq!(
                reader
                    .read_block(list.formats[0].id, 0, usize::MAX)
                    .unwrap()
                    .block,
                bytes
            );
        }
        // An invalid write is rejected before taking/replacing ownership.
        assert!(owner.write(&ClipKind::Text, &[0xff], &[]).is_err());
        assert_eq!(
            reader.list_formats().unwrap().formats[0].kind,
            ClipKind::Png
        );
        drop(owner);
        assert!(reader.list_formats().unwrap().formats.is_empty());

        // Creating/subscribing/polling does not request content; enumeration
        // requests TARGETS alone. The actual peer is independent of Worker.
        let external_text = "external café 🐟".as_bytes().to_vec();
        let peer = ExternalOwner::start(&display, external_text.clone(), ExternalResponse::Direct);
        let limited = X11Clipboard::connect_to_display(
            &display,
            X11ClipboardOptions {
                max_bytes: 128,
                transfer_timeout: Duration::from_millis(200),
            },
        )
        .unwrap();
        let _subscription = limited.subscribe_changes().unwrap();
        let _ = limited.sequence().unwrap();
        let metadata = limited.list_formats().unwrap();
        assert_eq!(*peer.requests.lock().unwrap(), vec![peer.targets]);
        let id = metadata.formats[0].id;
        assert_eq!(id, peer.utf8);
        assert_eq!(
            limited.read_block(id, 0, 0).unwrap().total_len,
            external_text.len() as u64
        );
        assert_eq!(
            limited.read_block(id, 0, usize::MAX).unwrap().block,
            external_text
        );
        assert_eq!(
            *peer.requests.lock().unwrap(),
            vec![peer.targets, peer.utf8]
        );
        drop(peer);

        for mode in [
            ExternalResponse::Direct,
            ExternalResponse::Incr,
            ExternalResponse::Stall,
        ] {
            let peer = ExternalOwner::start(&display, vec![b'x'; 256], mode);
            let list = limited.list_formats().unwrap();
            let error = limited
                .read_block(list.formats[0].id, 0, 0)
                .unwrap_err()
                .to_string();
            match mode {
                ExternalResponse::Stall => assert!(error.contains("timed out"), "{error}"),
                _ => assert!(error.contains("byte limit"), "{error}"),
            }
            drop(peer);
        }
        // A timed-out conversion does not poison a later owner's read.
        let _peer = ExternalOwner::start(&display, b"recovered".to_vec(), ExternalResponse::Direct);
        let list = limited.list_formats().unwrap();
        assert_eq!(
            limited
                .read_block(list.formats[0].id, 0, 128)
                .unwrap()
                .block,
            b"recovered"
        );
    }
}
