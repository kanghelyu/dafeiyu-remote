// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (C) 2025 Dafeiyu Remote Project

//! Windows user-mode half of the Dafeiyu virtual display (IddCx) control plane.
//!
//! # The two halves
//!
//! The actual monitor is produced by an IddCx **UMDF2** display driver
//! (`crates/dfy-vdisplay/driver/`, C, WDK build). That driver cannot be driven through
//! Win32 display APIs because an indirect display is not a real adapter; instead it
//! publishes a device interface and accepts control IOCTLs. This module is the other
//! half: it enumerates the interface, opens the device and speaks the protocol.
//!
//! ```text
//!   dfy-session / dfy-cli
//!        │  VirtualDisplayBackend (crate::VirtualDisplayBackend)
//!        ▼
//!   WinDriverBackend ──CreateFileW(\\.\...)──► UMDF2 IddCx driver
//!        │  DeviceIoControl(IOCTL_VDISPLAY_*)      │ IddCxMonitorCreate/Arrival
//!        │  WaitForSingleObject(named event)      │ EVT_IDD_CX_MONITOR_ASSIGN_SWAPCHAIN
//!        ▼                                        ▼
//!   Windows display stack (CCD / DXGI / DWM) sees one more monitor
//! ```
//!
//! # Shared ABI contract
//!
//! Everything in this section is a **hard ABI contract**: the C driver in `driver/`
//! mirrors these numbers and layouts byte for byte, so changing any of them requires a
//! protocol version bump on both sides at once.
//!
//! * Device interface class GUID: [`DEVICE_INTERFACE_GUID`]
//!   (`{54EC77FA-1377-44E6-8C32-88FD5F44C84C}`) — the same class GUID the upstream
//!   RustDesk/UU indirect display driver uses, so existing diagnostics keep working. The
//!   *device path* is unique to us (`\\?\root#dfyvdisplay#...`), and
//!   [`enumerate_device_interfaces`] returns every instance.
//! * All IOCTLs are `CTL_CODE(FILE_DEVICE_UNKNOWN, 0x800 + n, METHOD_BUFFERED,
//!   FILE_ANY_ACCESS)`, i.e. [`IOCTL_VDISPLAY_CREATE`] and friends. They are deliberately
//!   **not** the RustDesk numbers (`0x30C004/8/C`, `FILE_DEVICE_CHANGER`): our driver and
//!   their driver can be installed side by side without one interpreting the other's
//!   requests.
//! * Input/output structs are `#[repr(C)]`, naturally aligned, integer-only
//!   ([`VdisplayCreate`], [`VdisplayUpdate`], [`VdisplayDestroy`], [`VdisplayState`],
//!   [`VdisplayEvent`], [`VdisplayProtocolInfo`]). Sizes are asserted in unit tests; the
//!   driver `static_assert`s the same numbers.
//! * Every create returns a **display id**; the driver uses it as the `ConnectorIndex`
//!   (id 0..[`MAX_VIRTUAL_DISPLAYS`]-1) so a display keeps its identity for its lifetime
//!   across mode updates. [`INVALID_DISPLAY_ID`] means "rejected".
//! * Mode tables are pushed by the driver, not hard-coded: `IOCTL_VDISPLAY_CREATE` and
//!   `IOCTL_VDISPLAY_UPDATE` make the driver call `IddCxMonitorUpdateModes{,2}` for that
//!   connector only. Nothing here touches the physical monitors' modes.
//! * Hotplug notification uses a named event: the driver creates
//!   [`DEVICE_EVENT_NAME`] and signals it whenever its display set or mode table changes;
//!   user mode waits on it and then drains queued [`VdisplayEvent`] records with
//!   [`IOCTL_VDISPLAY_TAKE_EVENT`]. The event is only an optimization — [`list`] always
//!   works, so a consumer that cannot open the event (session-0 object namespace
//!   mismatch, access denied) degrades to polling instead of failing.
//!
//! [`list`]: crate::VirtualDisplayBackend::list
//!
//! # Error mapping
//!
//! `DeviceIoControl` failures surface as `anyhow::Error` with the Win32 text and the
//! IOCTL name, never as a panic. A handle that fails with a disconnect-style status
//! (`ERROR_INVALID_HANDLE`, `ERROR_FILE_NOT_FOUND`, `ERROR_DEVICE_NOT_CONNECTED`,
//! `ERROR_NO_SUCH_DEVICE` — what a driver unload/restart produces) is dropped so the next
//! call re-enumerates the interface; that is the user-mode half of the crash-recovery
//! story described in `design/020-virtual-display.md`.
//!
//! # Verifying against the driver headers
//!
//! The driver-side skeleton is not compiled in this phase, so the C prototypes in
//! `driver/driver.h` are reviewed by hand against WDK `iddcx.h`. When the driver is
//! first built, `static_assert(sizeof(VdisplayCreate) == 80, ...)` and friends in
//! `driver/driver.h` are the compile-time guard that this file and the driver agree.

use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::addr_of;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{anyhow, bail, Context, Result};
use windows::core::{GUID, HRESULT, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_NO_MORE_ITEMS, GENERIC_READ, GENERIC_WRITE, HANDLE, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Ioctl::FILE_DEVICE_UNKNOWN;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::System::IO::DeviceIoControl;

use crate::{VirtualDisplayBackend, VirtualDisplayInfo, VirtualDisplaySpec};

/// Device interface class GUID published by the Dafeiyu virtual display driver.
///
/// `{54EC77FA-1377-44E6-8C32-88FD5F44C84C}`.
pub const DEVICE_INTERFACE_GUID: GUID = GUID::from_u128(0x54EC77FA_1377_44E6_8C32_88FD5F44C84C);

/// Lowercase, brace-free rendering of [`DEVICE_INTERFACE_GUID`] for messages and for
/// matching device paths (`\\?\root#...#{54ec77fa-...}`).
pub const DEVICE_INTERFACE_GUID_STRING: &str = "54ec77fa-1377-44e6-8c32-88fd5f44c84c";

/// Name of the kernel-side event the driver signals when its display set or mode table
/// changes.
///
/// The event is auto-reset, so one waiter consumes one notification; the authoritative
/// state is always re-read with [`IOCTL_VDISPLAY_LIST`] and queued [`VdisplayEvent`]
/// records are drained with [`IOCTL_VDISPLAY_TAKE_EVENT`].
pub const DEVICE_EVENT_NAME: &str = "DafeiyuVDisplayDeviceEvent";

/// Protocol version negotiated through [`IOCTL_VDISPLAY_GET_PROTOCOL_INFO`].
///
/// Bump on any ABI change (struct layout, IOCTL number, semantics). A driver that answers
/// with a different version is treated as incompatible by
/// [`WinDriverBackend::is_available`]; call [`WinDriverBackend::protocol_info`] to tell
/// "not installed" from "installed but too old/new".
pub const PROTOCOL_VERSION: u32 = 1;

/// Capacity of the driver-side `name` buffer in [`VdisplayCreate`]: 63 printable bytes
/// plus the NUL terminator.
pub const VDISPLAY_NAME_MAX: usize = 64;

/// Maximum number of simultaneously existing virtual displays the driver exposes.
///
/// Matches `IDDCX_ADAPTER_CAPS.MaxMonitorsSupported` in the driver skeleton (8 connectors
/// per adapter, so 8 virtual monitors can be attached at once).
pub const MAX_VIRTUAL_DISPLAYS: usize = 8;

/// Display id returned by the driver for a rejected [`IOCTL_VDISPLAY_CREATE`].
pub const INVALID_DISPLAY_ID: u32 = u32::MAX;

/// `CTL_CODE` transfer method used by every IOCTL below.
const METHOD_BUFFERED: u32 = 0;
/// `CTL_CODE` access requirement used by every IOCTL below.
const FILE_ANY_ACCESS: u32 = 0;

/// Builds a Windows `CTL_CODE` value: `(DeviceType << 16) | (Access << 14) | (Function << 2)
/// | Method`.
const fn ctl_code(device_type: u32, function: u32, method: u32, access: u32) -> u32 {
    (device_type << 16) | (access << 14) | (function << 2) | method
}

/// Creates a virtual display. Input [`VdisplayCreate`], output `u32` display id.
///
/// `0x222000` = `CTL_CODE(FILE_DEVICE_UNKNOWN=0x22, 0x800, METHOD_BUFFERED, FILE_ANY_ACCESS)`.
pub const IOCTL_VDISPLAY_CREATE: u32 =
    ctl_code(FILE_DEVICE_UNKNOWN, 0x800, METHOD_BUFFERED, FILE_ANY_ACCESS);

/// Changes geometry/refresh/DPI of an existing display. Input [`VdisplayUpdate`], no output.
///
/// `0x222004`.
pub const IOCTL_VDISPLAY_UPDATE: u32 =
    ctl_code(FILE_DEVICE_UNKNOWN, 0x801, METHOD_BUFFERED, FILE_ANY_ACCESS);

/// Destroys a display (plug-out). Input [`VdisplayDestroy`], no output. `0x222008`.
pub const IOCTL_VDISPLAY_DESTROY: u32 =
    ctl_code(FILE_DEVICE_UNKNOWN, 0x802, METHOD_BUFFERED, FILE_ANY_ACCESS);

/// Reads the whole display set. Input none, output [`VdisplayStateList`]. `0x22200C`.
pub const IOCTL_VDISPLAY_LIST: u32 =
    ctl_code(FILE_DEVICE_UNKNOWN, 0x803, METHOD_BUFFERED, FILE_ANY_ACCESS);

/// Drains one queued hotplug record. Input none, output [`VdisplayEvent`]; a driver that
/// has no queued event returns success with zero bytes written. `0x222010`.
pub const IOCTL_VDISPLAY_TAKE_EVENT: u32 =
    ctl_code(FILE_DEVICE_UNKNOWN, 0x804, METHOD_BUFFERED, FILE_ANY_ACCESS);

/// Queries the ABI version and capability flags. Input none, output
/// [`VdisplayProtocolInfo`]. `0x222014`.
pub const IOCTL_VDISPLAY_GET_PROTOCOL_INFO: u32 =
    ctl_code(FILE_DEVICE_UNKNOWN, 0x805, METHOD_BUFFERED, FILE_ANY_ACCESS);

/// Win32 `ERROR_INVALID_HANDLE` (6); a stale handle after a driver restart.
const WIN32_ERROR_INVALID_HANDLE: u32 = 6;
/// Win32 `ERROR_FILE_NOT_FOUND` (2); the device path vanished.
const WIN32_ERROR_FILE_NOT_FOUND: u32 = 2;
/// Win32 `ERROR_NO_SUCH_DEVICE` (433); the device object is gone.
const WIN32_ERROR_NO_SUCH_DEVICE: u32 = 433;
/// Win32 `ERROR_DEVICE_NOT_CONNECTED` (1167); the device object is gone.
const WIN32_ERROR_DEVICE_NOT_CONNECTED: u32 = 1167;

/// Kinds carried in [`VdisplayEvent::kind`].
pub mod event_kind {
    /// A monitor became available to the desktop.
    pub const MONITOR_ARRIVAL: u32 = 1;
    /// A monitor was removed from the desktop.
    pub const MONITOR_DEPARTURE: u32 = 2;
    /// A monitor's mode table changed (resolution/refresh/DPI).
    pub const MODES_CHANGED: u32 = 3;

    /// Human readable name for a raw event kind, for logs.
    pub fn name(kind: u32) -> &'static str {
        match kind {
            MONITOR_ARRIVAL => "monitor-arrival",
            MONITOR_DEPARTURE => "monitor-departure",
            MODES_CHANGED => "modes-changed",
            _ => "unknown",
        }
    }
}

/// `IOCTL_VDISPLAY_CREATE` input.
///
/// Layout: 4 × `u32` + `char[64]` = 80 bytes, alignment 4, no padding.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VdisplayCreate {
    /// Requested width in pixels.
    pub width: u32,
    /// Requested height in pixels.
    pub height: u32,
    /// Requested vertical refresh rate in Hz.
    pub refresh_hz: u32,
    /// Requested monitor DPI.
    pub dpi: u32,
    /// NUL-terminated printable ASCII label; bytes past the terminator are zero.
    pub name: [u8; VDISPLAY_NAME_MAX],
}

impl VdisplayCreate {
    /// Packs a spec into the wire struct.
    ///
    /// `spec` should already be validated with
    /// [`VirtualDisplaySpec::validate`]; the name is nevertheless truncated at a UTF-8
    /// character boundary and NUL-terminated so a bad hint can never corrupt the ABI
    /// buffer.
    pub fn from_spec(spec: &VirtualDisplaySpec) -> Self {
        Self {
            width: spec.width,
            height: spec.height,
            refresh_hz: spec.refresh_hz,
            dpi: spec.dpi,
            name: encode_name_hint(&spec.name_hint),
        }
    }

    /// The label as a UTF-8 string, cut at the first NUL.
    pub fn name_str(&self) -> String {
        let end = self
            .name
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(self.name.len());
        String::from_utf8_lossy(&self.name[..end]).into_owned()
    }

    /// This struct as raw bytes for `DeviceIoControl`.
    fn as_bytes(&self) -> &[u8] {
        // SAFETY: `VdisplayCreate` is `#[repr(C)]` and contains only `u32` and `u8` fields,
        // so it has no padding and every byte of a live value is initialised. The slice
        // covers exactly `size_of::<Self>()` bytes and borrows `self`.
        unsafe { std::slice::from_raw_parts(self as *const Self as *const u8, size_of::<Self>()) }
    }
}

/// Encodes a name hint into the fixed `char[64]` field: truncate at a character boundary,
/// then NUL-terminate by leaving the rest zeroed.
fn encode_name_hint(name_hint: &str) -> [u8; VDISPLAY_NAME_MAX] {
    let mut buffer = [0u8; VDISPLAY_NAME_MAX];
    let mut written = 0usize;
    for ch in name_hint.chars() {
        let mut encoded = [0u8; 4];
        let bytes = ch.encode_utf8(&mut encoded).as_bytes();
        if written + bytes.len() >= VDISPLAY_NAME_MAX {
            break;
        }
        buffer[written..written + bytes.len()].copy_from_slice(bytes);
        written += bytes.len();
    }
    buffer
}

/// `IOCTL_VDISPLAY_UPDATE` input: 5 × `u32` = 20 bytes, alignment 4.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VdisplayUpdate {
    /// Target display id (the driver's `ConnectorIndex`).
    pub id: u32,
    /// New width in pixels.
    pub width: u32,
    /// New height in pixels.
    pub height: u32,
    /// New vertical refresh rate in Hz.
    pub refresh_hz: u32,
    /// New monitor DPI.
    pub dpi: u32,
}

impl VdisplayUpdate {
    /// Packs an id and spec into the wire struct.
    pub fn new(id: u32, spec: &VirtualDisplaySpec) -> Self {
        Self {
            id,
            width: spec.width,
            height: spec.height,
            refresh_hz: spec.refresh_hz,
            dpi: spec.dpi,
        }
    }

    /// This struct as raw bytes for `DeviceIoControl`.
    fn as_bytes(&self) -> &[u8] {
        // SAFETY: `#[repr(C)]` struct of five `u32` fields: no padding, fully initialised,
        // slice bounded by `size_of::<Self>()`.
        unsafe { std::slice::from_raw_parts(self as *const Self as *const u8, size_of::<Self>()) }
    }
}

/// `IOCTL_VDISPLAY_DESTROY` input: one `u32` = 4 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VdisplayDestroy {
    /// Display id to remove.
    pub id: u32,
}

impl VdisplayDestroy {
    /// Wraps an id.
    pub fn new(id: u32) -> Self {
        Self { id }
    }

    /// This struct as raw bytes for `DeviceIoControl`.
    fn as_bytes(&self) -> &[u8] {
        // SAFETY: `#[repr(C)]` struct with a single `u32`: no padding, fully initialised.
        unsafe { std::slice::from_raw_parts(self as *const Self as *const u8, size_of::<Self>()) }
    }
}

/// `IOCTL_VDISPLAY_TAKE_EVENT` output: 2 × `u32` = 8 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VdisplayEvent {
    /// One of the [`event_kind`] constants.
    pub kind: u32,
    /// Display the event refers to.
    pub id: u32,
}

/// `IOCTL_VDISPLAY_LIST` output header: entries used plus a reserved word for extension.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VdisplayStateListHeader {
    /// Number of valid entries in [`VdisplayStateList::states`] (0..=[`MAX_VIRTUAL_DISPLAYS`]).
    pub count: u32,
    /// Reserved; must be zero.
    pub reserved: u32,
}

/// One entry of the [`VdisplayStateList`] array: 6 × `u32` = 24 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VdisplayState {
    /// Display id (`ConnectorIndex`) or [`INVALID_DISPLAY_ID`] for an unused slot.
    pub id: u32,
    /// Current width in pixels.
    pub width: u32,
    /// Current height in pixels.
    pub height: u32,
    /// Current vertical refresh rate in Hz.
    pub refresh_hz: u32,
    /// Current monitor DPI.
    pub dpi: u32,
    /// Non-zero when the monitor is attached to the desktop.
    pub attached: u32,
}

/// `IOCTL_VDISPLAY_LIST` output: header + a fixed array of [`VdisplayState`].
///
/// The size is fixed ([`MAX_VIRTUAL_DISPLAYS`] entries) instead of "call twice with a
/// growing buffer": the driver never has more connectors, so one round trip is enough and
/// a truncated reply is impossible.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct VdisplayStateList {
    /// How many entries are valid.
    pub header: VdisplayStateListHeader,
    /// Display state slots; entries past `header.count` are zero.
    pub states: [VdisplayState; MAX_VIRTUAL_DISPLAYS],
}

impl VdisplayStateList {
    /// All-zero instance, valid because every field is an integer.
    pub fn zeroed() -> Self {
        // SAFETY: `VdisplayStateList` is `#[repr(C)]` and made only of `u32` fields and
        // arrays of such structs; the all-zero bit pattern is valid for all of them.
        unsafe { zeroed() }
    }

    /// Mutable view of the struct as bytes, for `DeviceIoControl`.
    fn as_bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: `#[repr(C)]`, integer-only, no padding (header 8 + 8 × 24 = 200 bytes),
        // and the slice covers exactly `size_of::<Self>()` bytes of a live value.
        unsafe { std::slice::from_raw_parts_mut(self as *mut Self as *mut u8, size_of::<Self>()) }
    }
}

impl Default for VdisplayStateList {
    fn default() -> Self {
        Self::zeroed()
    }
}

/// `IOCTL_VDISPLAY_GET_PROTOCOL_INFO` output: 2 × `u32` = 8 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VdisplayProtocolInfo {
    /// ABI version; see [`PROTOCOL_VERSION`].
    pub protocol_version: u32,
    /// Bitmask of [`crate::capability`] flags the driver supports.
    pub capabilities: u32,
}

/// Whether `path` looks like an instance of our device interface (matches the class GUID
/// and is a `\\?\` device path).
///
/// Used for diagnostics and in unit tests; the authoritative enumeration is
/// [`enumerate_device_interfaces`].
pub fn looks_like_device_path(path: &str) -> bool {
    path.starts_with(r"\\?\")
        && path
            .to_ascii_lowercase()
            .contains(DEVICE_INTERFACE_GUID_STRING)
}

/// Enumerates every present instance of the Dafeiyu virtual display device interface.
///
/// Returns the device paths accepted by `CreateFileW`. An empty result means the driver is
/// not installed, or installed but not started (install/uninstall/restart is documented in
/// `crates/dfy-vdisplay/driver/README.md`). This function is public so a service watchdog
/// can distinguish "driver missing" from "handle went stale" in its logs.
pub fn enumerate_device_interfaces() -> Result<Vec<String>> {
    let mut paths = Vec::new();
    let mut detail_errors = Vec::new();

    // SAFETY: the SetupDi calls are given a valid HDEVINFO, a `SP_DEVICE_INTERFACE_DATA`
    // whose `cbSize` is set to its own size, and a detail buffer sized from the byte count
    // the API itself reported. The buffer is over-allocated as `u64` words so the
    // `SP_DEVICE_INTERFACE_DETAIL_DATA_W` view is pointer-aligned.
    unsafe {
        let hdev = SetupDiGetClassDevsW(
            Some(&DEVICE_INTERFACE_GUID),
            PCWSTR::null(),
            None,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
        .context("SetupDiGetClassDevsW for the Dafeiyu virtual display device interface")?;
        let devices = DevInfoSet(hdev);

        let mut index = 0u32;
        loop {
            let mut interface = zeroed::<SP_DEVICE_INTERFACE_DATA>();
            interface.cbSize = size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
            match SetupDiEnumDeviceInterfaces(
                devices.0,
                None,
                &DEVICE_INTERFACE_GUID,
                index,
                &mut interface,
            ) {
                Ok(()) => {}
                Err(err) if err.code() == HRESULT::from_win32(ERROR_NO_MORE_ITEMS.0) => break,
                Err(err) => {
                    return Err(err)
                        .with_context(|| format!("SetupDiEnumDeviceInterfaces(index {index})"));
                }
            }
            index += 1;
            match device_interface_path(devices.0, &interface) {
                Ok(path) => paths.push(path),
                Err(err) => detail_errors.push(err.to_string()),
            }
        }
    }

    if paths.is_empty() {
        let detail = if detail_errors.is_empty() {
            String::new()
        } else {
            format!(" (interface detail errors: {})", detail_errors.join("; "))
        };
        return Err(anyhow!(
            "no Dafeiyu virtual display device interface {{{DEVICE_INTERFACE_GUID_STRING}}} is \
             present{detail}; the driver is not installed or not started"
        ));
    }
    Ok(paths)
}

/// Owns a `HDEVINFO` so it is destroyed on every exit path.
struct DevInfoSet(HDEVINFO);

impl Drop for DevInfoSet {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from `SetupDiGetClassDevsW` and is not used afterwards.
        unsafe {
            let _ = SetupDiDestroyDeviceInfoList(self.0);
        }
    }
}

/// Reads the device path of one enumerated interface.
///
/// # Safety
///
/// `hdev` must be a live `HDEVINFO` from `SetupDiGetClassDevsW` and `interface` a filled
/// `SP_DEVICE_INTERFACE_DATA` belonging to it (which is how [`enumerate_device_interfaces`]
/// calls this).
unsafe fn device_interface_path(
    hdev: HDEVINFO,
    interface: &SP_DEVICE_INTERFACE_DATA,
) -> Result<String> {
    let mut required = 0u32;
    // The first call is expected to fail with ERROR_INSUFFICIENT_BUFFER, but it fills in
    // `required`; the return value itself is not interesting.
    let _ = SetupDiGetDeviceInterfaceDetailW(hdev, interface, None, 0, Some(&mut required), None);

    let fixed_size = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
    if (required as usize) < fixed_size {
        bail!(
            "SetupDiGetDeviceInterfaceDetailW reported a {required}-byte buffer, smaller than \
             the {fixed_size}-byte fixed part"
        );
    }

    // `SP_DEVICE_INTERFACE_DETAIL_DATA_W` is a variable-length struct: the fixed part is
    // `cbSize` plus (on 64-bit) 4 bytes of padding, then the UTF-16 path. `u64` words keep
    // the allocation 8-byte aligned.
    let mut buffer = vec![0u64; (required as usize).div_ceil(size_of::<u64>())];
    let detail = buffer.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
    (*detail).cbSize = fixed_size as u32;
    SetupDiGetDeviceInterfaceDetailW(
        hdev,
        interface,
        Some(detail),
        required,
        Some(&mut required),
        None,
    )
    .context("SetupDiGetDeviceInterfaceDetailW")?;

    // The path starts right after `cbSize` (offset 4 in both 32- and 64-bit layouts).
    let path = addr_of!((*detail).DevicePath) as *const u16;
    let max_chars = (required as usize).saturating_sub(4) / 2;
    let mut len = 0usize;
    while len < max_chars && *path.add(len) != 0 {
        len += 1;
    }
    let utf16 = std::slice::from_raw_parts(path, len);
    Ok(String::from_utf16_lossy(utf16))
}

/// A Win32 handle with ownership, plus the `Send`/`Sync` the trait needs.
struct OwnedHandle(HANDLE);

// SAFETY: a Win32 handle is a process-wide reference to a kernel object; the value itself
// may be moved between threads (which is what `CreateFileW` returning it implies). The
// operations this crate performs on it (`DeviceIoControl`, `WaitForSingleObject`,
// `CloseHandle`) are thread-safe for these object types and are serialised by the owning
// `Mutex`, and `OwnedHandle` is never cloned, so the handle is closed exactly once.
unsafe impl Send for OwnedHandle {}
// SAFETY: see above; shared references are only used to issue thread-safe Win32 calls.
unsafe impl Sync for OwnedHandle {}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a valid handle owned by this struct and is not used after
        // this point; a close failure on exit is not actionable.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

impl OwnedHandle {
    /// Raw handle value for Win32 calls.
    fn raw(&self) -> HANDLE {
        self.0
    }

    /// One `DeviceIoControl` round trip.
    fn ioctl(&self, code: u32, input: Option<&[u8]>, mut output: Option<&mut [u8]>) -> Result<u32> {
        let (input_ptr, input_len) = match input {
            Some(bytes) => (Some(bytes.as_ptr() as *const c_void), bytes.len() as u32),
            None => (None, 0),
        };
        let (output_ptr, output_len) = match output.as_mut() {
            Some(bytes) => (Some(bytes.as_mut_ptr() as *mut c_void), bytes.len() as u32),
            None => (None, 0),
        };
        let mut returned = 0u32;
        // SAFETY: the pointers/sizes come from live slices that outlive the call (all
        // parameters are `&self`-borrowed locals or the caller's buffers) and match the
        // METHOD_BUFFERED contract; a null pointer is passed only with length 0. The handle
        // is valid because it is owned by `self`.
        unsafe {
            DeviceIoControl(
                self.0,
                code,
                input_ptr,
                input_len,
                output_ptr,
                output_len,
                Some(&mut returned),
                None,
            )
            .with_context(|| format!("DeviceIoControl({})", ioctl_name(code)))?;
        }
        Ok(returned)
    }
}

/// Human readable IOCTL name for error messages.
fn ioctl_name(code: u32) -> &'static str {
    match code {
        IOCTL_VDISPLAY_CREATE => "IOCTL_VDISPLAY_CREATE",
        IOCTL_VDISPLAY_UPDATE => "IOCTL_VDISPLAY_UPDATE",
        IOCTL_VDISPLAY_DESTROY => "IOCTL_VDISPLAY_DESTROY",
        IOCTL_VDISPLAY_LIST => "IOCTL_VDISPLAY_LIST",
        IOCTL_VDISPLAY_TAKE_EVENT => "IOCTL_VDISPLAY_TAKE_EVENT",
        IOCTL_VDISPLAY_GET_PROTOCOL_INFO => "IOCTL_VDISPLAY_GET_PROTOCOL_INFO",
        _ => "unknown IOCTL",
    }
}

/// Whether a Win32 status means "the device object disappeared" (driver unload, restart,
/// surprise removal) and the cached handle must be dropped.
fn is_disconnect_status(err: &windows::core::Error) -> bool {
    let code = err.code();
    [
        WIN32_ERROR_INVALID_HANDLE,
        WIN32_ERROR_FILE_NOT_FOUND,
        WIN32_ERROR_NO_SUCH_DEVICE,
        WIN32_ERROR_DEVICE_NOT_CONNECTED,
    ]
    .into_iter()
    .any(|status| code == HRESULT::from_win32(status))
}

/// An opened device interface plus the optional hotplug notification event.
struct OpenedDevice {
    device: OwnedHandle,
    event: Option<Arc<OwnedHandle>>,
}

/// Opens the first present driver interface instance and its notification event.
fn open_device_interface() -> Result<OpenedDevice> {
    let path = enumerate_device_interfaces()?
        .into_iter()
        .next()
        .ok_or_else(|| {
            anyhow!("the virtual display device interface vanished during enumeration")
        })?;
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` is a NUL-terminated UTF-16 buffer that outlives the call, the access
    // and share modes are valid for a device interface, and `OPEN_EXISTING` requires no
    // creation-time security descriptor.
    let device = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            (GENERIC_READ | GENERIC_WRITE).0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
        .with_context(|| format!("CreateFileW({path})"))?
    };

    // A missing event only degrades hotplug notifications to polling, so it is not fatal.
    let event = create_device_event().ok().map(Arc::new);
    Ok(OpenedDevice {
        device: OwnedHandle(device),
        event,
    })
}

/// Opens (or creates, if the driver has not yet) the named notification event.
fn create_device_event() -> Result<OwnedHandle> {
    let wide: Vec<u16> = DEVICE_EVENT_NAME
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: auto-reset, initially unsignalled, NUL-terminated name that outlives the call.
    let handle = unsafe { CreateEventW(None, false, false, PCWSTR(wide.as_ptr())) }
        .with_context(|| format!("CreateEventW({DEVICE_EVENT_NAME})"))?;
    Ok(OwnedHandle(handle))
}

/// Lazily opened connection state, all of it behind one mutex.
struct Connection {
    device: Option<OwnedHandle>,
    event: Option<Arc<OwnedHandle>>,
    /// Id → creation name hint, because the driver stores the monitor name in the EDID but
    /// [`VdisplayState`] does not carry it back.
    names: HashMap<u32, String>,
}

impl Connection {
    /// Opens the device interface if this is the first call, then returns the handle.
    fn device(&mut self) -> Result<&OwnedHandle> {
        if self.device.is_none() {
            let opened = open_device_interface()?;
            self.device = Some(opened.device);
            self.event = opened.event;
        }
        self.device
            .as_ref()
            .ok_or_else(|| anyhow!("virtual display device handle disappeared"))
    }

    /// `DeviceIoControl`, dropping the cached handle when the device object disappeared.
    fn ioctl(&mut self, code: u32, input: Option<&[u8]>, output: Option<&mut [u8]>) -> Result<u32> {
        let result = self.device()?.ioctl(code, input, output);
        if let Err(err) = &result {
            let disconnected = err
                .downcast_ref::<windows::core::Error>()
                .is_some_and(is_disconnect_status);
            if disconnected {
                // Force re-enumeration (new device instance, or the same one after a
                // driver restart): the next call opens a fresh handle.
                self.device = None;
                self.event = None;
            }
        }
        result
    }

    /// Protocol info query (also the liveness probe).
    fn protocol_info(&mut self) -> Result<VdisplayProtocolInfo> {
        let mut info = VdisplayProtocolInfo::default();
        let written = self.ioctl(
            IOCTL_VDISPLAY_GET_PROTOCOL_INFO,
            None,
            Some(unsafe {
                // SAFETY: `info` is a live, integer-only struct; the slice covers exactly
                // `size_of::<VdisplayProtocolInfo>()` writable bytes.
                std::slice::from_raw_parts_mut(
                    &mut info as *mut VdisplayProtocolInfo as *mut u8,
                    size_of::<VdisplayProtocolInfo>(),
                )
            }),
        )?;
        if written as usize != size_of::<VdisplayProtocolInfo>() {
            bail!(
                "driver answered IOCTL_VDISPLAY_GET_PROTOCOL_INFO with {written} bytes, \
                 expected {}",
                size_of::<VdisplayProtocolInfo>()
            );
        }
        Ok(info)
    }
}

/// Windows backend: talks to the Dafeiyu IddCx driver through its device interface.
///
/// The device is opened lazily on first use, so constructing the backend never touches the
/// OS and a machine without the driver still gets a working value whose
/// [`VirtualDisplayBackend::is_available`] is `false`.
pub struct WinDriverBackend {
    connection: Mutex<Connection>,
}

impl WinDriverBackend {
    /// Creates the backend with no open device handle.
    pub fn new() -> Self {
        Self {
            connection: Mutex::new(Connection {
                device: None,
                event: None,
                names: HashMap::new(),
            }),
        }
    }

    /// Locks the connection state, mapping mutex poisoning to an error instead of panicking.
    fn lock(&self) -> Result<MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| anyhow!("virtual display backend state is poisoned by a panicking caller"))
    }

    /// Queries ABI version and capability flags.
    ///
    /// Unlike [`VirtualDisplayBackend::is_available`] this reports a version mismatch as
    /// data rather than collapsing it into `false`.
    pub fn protocol_info(&self) -> Result<VdisplayProtocolInfo> {
        self.lock()?.protocol_info()
    }

    /// Drops cached handles so the next operation re-enumerates the device interface.
    ///
    /// A service watchdog calls this after it observes a driver restart (device interface
    /// GUID reappears with a new instance, or an IOCTL failed with a disconnect status);
    /// the display set itself is re-read from the driver, so ids may change.
    pub fn reconnect(&self) -> Result<()> {
        let mut connection = self.lock()?;
        connection.device = None;
        connection.event = None;
        Ok(())
    }

    /// Waits up to `timeout_ms` for the driver to signal a display-set or mode-table change,
    /// then drains one queued [`VdisplayEvent`].
    ///
    /// Returns `Ok(None)` on timeout and when the driver has nothing queued (several changes
    /// coalesced by the auto-reset event). The notification event is created by the driver
    /// in its own object namespace; if user mode cannot open it (for example because the
    /// consumer runs in a different session than the driver host), this returns an error and
    /// callers should poll [`VirtualDisplayBackend::list`] instead.
    pub fn wait_for_change(&self, timeout_ms: u32) -> Result<Option<VdisplayEvent>> {
        let event = {
            let mut connection = self.lock()?;
            connection.device()?; // ensure the device, and therefore the event, is opened
            match connection.event.clone() {
                Some(event) => event,
                None => bail!(
                    "the driver's {DEVICE_EVENT_NAME} notification event is not available; \
                     poll list() instead"
                ),
            }
        };

        // SAFETY: `event` is an owned, live event handle kept alive by the `Arc` for the
        // whole wait; the timeout is a plain millisecond count.
        let status = unsafe { WaitForSingleObject(event.raw(), timeout_ms) };
        if status == WAIT_OBJECT_0 {
            self.take_event()
        } else if status == WAIT_TIMEOUT {
            Ok(None)
        } else {
            bail!("WaitForSingleObject({DEVICE_EVENT_NAME}) returned unexpected status {status:?}")
        }
    }

    /// Drains one queued hotplug record without waiting.
    pub fn take_event(&self) -> Result<Option<VdisplayEvent>> {
        let mut connection = self.lock()?;
        let mut event = VdisplayEvent::default();
        let written = connection.ioctl(
            IOCTL_VDISPLAY_TAKE_EVENT,
            None,
            Some(unsafe {
                // SAFETY: `event` is a live, integer-only struct and the slice covers exactly
                // its size.
                std::slice::from_raw_parts_mut(
                    &mut event as *mut VdisplayEvent as *mut u8,
                    size_of::<VdisplayEvent>(),
                )
            }),
        )?;
        if written == 0 {
            Ok(None)
        } else if written as usize == size_of::<VdisplayEvent>() {
            Ok(Some(event))
        } else {
            bail!(
                "driver answered IOCTL_VDISPLAY_TAKE_EVENT with {written} bytes, expected 0 or {}",
                size_of::<VdisplayEvent>()
            )
        }
    }

    /// Reads the full display set from the driver (used by [`VirtualDisplayBackend::list`]).
    fn read_states(&self) -> Result<(Vec<VdisplayState>, HashMap<u32, String>)> {
        let mut connection = self.lock()?;
        let mut list = VdisplayStateList::zeroed();
        let written = connection.ioctl(IOCTL_VDISPLAY_LIST, None, Some(list.as_bytes_mut()))?;

        if written == 0 {
            bail!(
                "driver answered IOCTL_VDISPLAY_LIST with no data; it does not implement the \
                 list request (protocol {PROTOCOL_VERSION} expects {})",
                size_of::<VdisplayStateList>()
            );
        }
        if written as usize != size_of::<VdisplayStateList>() {
            bail!(
                "driver answered IOCTL_VDISPLAY_LIST with {written} bytes, expected {}",
                size_of::<VdisplayStateList>()
            );
        }
        let count = list.header.count as usize;
        if count > MAX_VIRTUAL_DISPLAYS {
            bail!(
                "driver reported {count} virtual displays, protocol maximum is \
                 {MAX_VIRTUAL_DISPLAYS}"
            );
        }
        let states = list.states[..count]
            .iter()
            .filter(|state| state.id != INVALID_DISPLAY_ID)
            .copied()
            .collect::<Vec<_>>();
        Ok((states, connection.names.clone()))
    }
}

impl Default for WinDriverBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl VirtualDisplayBackend for WinDriverBackend {
    fn list(&self) -> Result<Vec<VirtualDisplayInfo>> {
        let (states, names) = self.read_states()?;
        Ok(states
            .into_iter()
            .map(|state| VirtualDisplayInfo {
                id: state.id,
                spec: VirtualDisplaySpec {
                    width: state.width,
                    height: state.height,
                    refresh_hz: state.refresh_hz,
                    dpi: state.dpi,
                    name_hint: names.get(&state.id).cloned().unwrap_or_default(),
                },
                attached: state.attached != 0,
            })
            .collect())
    }

    fn create(&self, spec: &VirtualDisplaySpec) -> Result<u32> {
        spec.validate()?;
        let request = VdisplayCreate::from_spec(spec);
        let mut raw_id = [0u8; 4];
        let mut connection = self.lock()?;
        let written = connection.ioctl(
            IOCTL_VDISPLAY_CREATE,
            Some(request.as_bytes()),
            Some(&mut raw_id),
        )?;
        if written as usize != size_of::<u32>() {
            bail!(
                "driver answered IOCTL_VDISPLAY_CREATE with {written} bytes, expected {}",
                size_of::<u32>()
            );
        }
        let id = u32::from_ne_bytes(raw_id);
        if id == INVALID_DISPLAY_ID {
            bail!(
                "driver rejected the virtual display ({:?}) at protocol {PROTOCOL_VERSION}",
                spec
            );
        }
        connection.names.insert(id, spec.name_hint.clone());
        Ok(id)
    }

    fn update(&self, id: u32, spec: &VirtualDisplaySpec) -> Result<()> {
        if id == INVALID_DISPLAY_ID {
            bail!("display id {id:#x} is not a valid virtual display id");
        }
        spec.validate()?;
        let request = VdisplayUpdate::new(id, spec);
        let mut connection = self.lock()?;
        connection
            .ioctl(IOCTL_VDISPLAY_UPDATE, Some(request.as_bytes()), None)
            .with_context(|| format!("updating virtual display {id} to {:?}", spec))?;
        Ok(())
    }

    fn destroy(&self, id: u32) -> Result<()> {
        if id == INVALID_DISPLAY_ID {
            bail!("display id {id:#x} is not a valid virtual display id");
        }
        let request = VdisplayDestroy::new(id);
        let mut connection = self.lock()?;
        connection
            .ioctl(IOCTL_VDISPLAY_DESTROY, Some(request.as_bytes()), None)
            .with_context(|| format!("destroying virtual display {id}"))?;
        connection.names.remove(&id);
        Ok(())
    }

    fn is_available(&self) -> bool {
        match self.protocol_info() {
            Ok(info) => info.protocol_version == PROTOCOL_VERSION,
            Err(_) => false,
        }
    }

    fn capabilities(&self) -> u32 {
        match self.protocol_info() {
            Ok(info) => info.capabilities,
            Err(_) => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NAME_HINT_MAX_BYTES;

    #[test]
    fn device_interface_guid_is_the_contract_value() {
        assert_eq!(
            DEVICE_INTERFACE_GUID,
            GUID::from_u128(0x54EC77FA_1377_44E6_8C32_88FD5F44C84C)
        );
        assert_eq!(DEVICE_INTERFACE_GUID_STRING.len(), 36);
        assert!(DEVICE_INTERFACE_GUID_STRING.starts_with("54ec77fa-"));
    }

    #[test]
    fn ioctl_numbers_are_ctl_code_0x800_block_with_file_device_unknown() {
        // CTL_CODE(0x22, 0x800+n, buffered, any) = 0x22 << 16 | (0x800+n) << 2.
        assert_eq!(IOCTL_VDISPLAY_CREATE, 0x0022_2000);
        assert_eq!(IOCTL_VDISPLAY_UPDATE, 0x0022_2004);
        assert_eq!(IOCTL_VDISPLAY_DESTROY, 0x0022_2008);
        assert_eq!(IOCTL_VDISPLAY_LIST, 0x0022_200C);
        assert_eq!(IOCTL_VDISPLAY_TAKE_EVENT, 0x0022_2010);
        assert_eq!(IOCTL_VDISPLAY_GET_PROTOCOL_INFO, 0x0022_2014);
        // Deliberately distinct from the RustDesk/UU numbers so both drivers can coexist.
        assert_ne!(IOCTL_VDISPLAY_CREATE, 0x30C004);
        assert_eq!(FILE_DEVICE_UNKNOWN, 0x22);
    }

    #[test]
    fn wire_structs_have_the_sizes_the_driver_static_asserts() {
        assert_eq!(size_of::<VdisplayCreate>(), 80);
        assert_eq!(
            VdisplayCreate::from_spec(&VirtualDisplaySpec::hd_1080p())
                .as_bytes()
                .len(),
            80
        );
        assert_eq!(size_of::<VdisplayUpdate>(), 20);
        assert_eq!(size_of::<VdisplayDestroy>(), 4);
        assert_eq!(size_of::<VdisplayEvent>(), 8);
        assert_eq!(size_of::<VdisplayStateListHeader>(), 8);
        assert_eq!(size_of::<VdisplayState>(), 24);
        assert_eq!(
            size_of::<VdisplayStateList>(),
            size_of::<VdisplayStateListHeader>()
                + MAX_VIRTUAL_DISPLAYS * size_of::<VdisplayState>()
        );
        assert_eq!(size_of::<VdisplayStateList>(), 200);
        assert_eq!(size_of::<VdisplayProtocolInfo>(), 8);

        for alignment in [
            std::mem::align_of::<VdisplayCreate>(),
            std::mem::align_of::<VdisplayUpdate>(),
            std::mem::align_of::<VdisplayDestroy>(),
            std::mem::align_of::<VdisplayEvent>(),
            std::mem::align_of::<VdisplayState>(),
            std::mem::align_of::<VdisplayStateList>(),
            std::mem::align_of::<VdisplayProtocolInfo>(),
        ] {
            assert_eq!(alignment, 4, "ABI structs must stay u32-aligned");
        }
    }

    #[test]
    fn create_request_round_trips_the_spec() {
        let spec = VirtualDisplaySpec::new(2560, 1440, 120, 144, "dfy-virtual-1");
        let request = VdisplayCreate::from_spec(&spec);
        assert_eq!(request.width, 2560);
        assert_eq!(request.height, 1440);
        assert_eq!(request.refresh_hz, 120);
        assert_eq!(request.dpi, 144);
        assert_eq!(request.name_str(), "dfy-virtual-1");
        assert_eq!(
            &request.name[.."dfy-virtual-1".len()],
            b"dfy-virtual-1",
            "name hint must be stored verbatim"
        );
        assert_eq!(
            request.name["dfy-virtual-1".len()],
            0,
            "name must be NUL-terminated"
        );
    }

    #[test]
    fn name_hint_is_nul_terminated_and_truncated_at_a_char_boundary() {
        let long = "x".repeat(200);
        let spec = VirtualDisplaySpec::new(1920, 1080, 60, 96, long);
        let request = VdisplayCreate::from_spec(&spec);
        let stored = request.name_str();
        assert_eq!(stored.len(), NAME_HINT_MAX_BYTES);
        assert_eq!(request.name[NAME_HINT_MAX_BYTES], 0);

        // A multi-byte character that would straddle the limit is dropped whole, never split.
        let spec = VirtualDisplaySpec::new(1920, 1080, 60, 96, "a".repeat(62) + "屏");
        let request = VdisplayCreate::from_spec(&spec);
        assert_eq!(request.name_str(), "a".repeat(62));
        assert_eq!(request.name[62], 0);
    }

    #[test]
    fn update_and_destroy_requests_carry_their_ids() {
        let spec = VirtualDisplaySpec::new(1280, 720, 30, 120, "ignored");
        let update = VdisplayUpdate::new(7, &spec);
        assert_eq!(update.id, 7);
        assert_eq!((update.width, update.height), (1280, 720));
        assert_eq!(update.as_bytes().len(), 20);
        assert_eq!(VdisplayDestroy::new(7).as_bytes(), &7u32.to_ne_bytes());
    }

    #[test]
    fn state_list_zeroed_has_no_display_and_expected_byte_view() {
        let mut list = VdisplayStateList::zeroed();
        assert_eq!(list.header.count, 0);
        assert_eq!(list.states.len(), MAX_VIRTUAL_DISPLAYS);
        assert_eq!(list.as_bytes_mut().len(), 200);
        assert!(list.states.iter().all(|state| state.id == 0));
    }

    #[test]
    fn device_path_matching_accepts_our_interface_and_rejects_others() {
        assert!(looks_like_device_path(
            r"\\?\root#dfyvdisplay#0000#{54ec77fa-1377-44e6-8c32-88fd5f44c84c}"
        ));
        assert!(looks_like_device_path(&format!(
            r"\\?\ROOT#DFYVDISPLAY#0001#{{{}}}",
            DEVICE_INTERFACE_GUID_STRING
        )));
        assert!(!looks_like_device_path(
            r"\\?\root#sudovda#0000#{e5bcc234-1e0c-418a-a0d4-ef8b7501414d}"
        ));
        assert!(!looks_like_device_path(r"\\.\DISPLAY1"));
    }

    #[test]
    fn event_kinds_have_stable_names() {
        assert_eq!(
            event_kind::name(event_kind::MONITOR_ARRIVAL),
            "monitor-arrival"
        );
        assert_eq!(
            event_kind::name(event_kind::MONITOR_DEPARTURE),
            "monitor-departure"
        );
        assert_eq!(event_kind::name(event_kind::MODES_CHANGED), "modes-changed");
        assert_eq!(event_kind::name(999), "unknown");
    }

    #[test]
    fn ioctl_names_cover_every_opcode() {
        for code in [
            IOCTL_VDISPLAY_CREATE,
            IOCTL_VDISPLAY_UPDATE,
            IOCTL_VDISPLAY_DESTROY,
            IOCTL_VDISPLAY_LIST,
            IOCTL_VDISPLAY_TAKE_EVENT,
            IOCTL_VDISPLAY_GET_PROTOCOL_INFO,
        ] {
            assert_ne!(ioctl_name(code), "unknown IOCTL");
        }
        assert_eq!(ioctl_name(0x1234_5678), "unknown IOCTL");
    }

    #[test]
    fn disconnect_statuses_are_recognised() {
        for status in [
            WIN32_ERROR_INVALID_HANDLE,
            WIN32_ERROR_FILE_NOT_FOUND,
            WIN32_ERROR_NO_SUCH_DEVICE,
            WIN32_ERROR_DEVICE_NOT_CONNECTED,
        ] {
            let err = windows::core::Error::from_hresult(HRESULT::from_win32(status));
            assert!(
                is_disconnect_status(&err),
                "status {status} must force reconnect"
            );
        }
        let other = windows::core::Error::from_hresult(HRESULT::from_win32(87));
        assert!(!is_disconnect_status(&other));
    }

    #[test]
    fn backend_is_send_sync_and_degrades_without_a_driver() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<WinDriverBackend>();

        let backend = WinDriverBackend::new();
        // Without the driver these must be errors, not panics, and must mention the fix.
        let err = backend.list().unwrap_err().to_string();
        assert!(err.contains("device interface"), "{err}");
        assert!(backend.protocol_info().is_err());
        assert!(!backend.is_available());
        assert_eq!(backend.capabilities(), 0);
        assert!(!backend
            .wait_for_change(0)
            .expect_err("without a driver the notification event cannot be opened")
            .to_string()
            .is_empty());
        backend.reconnect().expect("reconnect must not fail");
    }
}
