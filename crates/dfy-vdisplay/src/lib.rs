// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (C) 2025 Dafeiyu Remote Project

//! Cross-platform virtual display control plane for Dafeiyu Remote.
//!
//! This crate is the *independent core module* that lets the session layer create
//! extra headless desktops ("virtual displays") and change their resolution without
//! touching the physical monitors. It exposes one OS-agnostic trait and one factory;
//! everything OS specific stays behind that boundary.
//!
//! # Platform matrix
//!
//! | OS | mechanism | state in this phase |
//! |----|-----------|----------------------|
//! | Windows | our own IddCx (UMDF2) indirect display driver; this crate implements the **user-mode half** (device-interface enumeration + IOCTL control plane) | implemented, requires `dfy-vdisplay` driver from `driver/` |
//! | Linux | one isolated Xvfb X server per desktop (no root; X11 clients only) | implemented; requires `Xvfb`, `xdpyinfo`, and optionally `xrandr` |
//! | macOS | `CGVirtualDisplay` (private CoreGraphics API) | stub, phase 2 |
//!
//! # Threading model
//!
//! [`VirtualDisplayBackend`] methods are **synchronous** and `Send + Sync`. Backends
//! serialise their control channel and state internally, and calls may block while an OS
//! operation or health probe completes. Async callers must wrap calls in
//! `tokio::task::spawn_blocking` instead of awaiting them on a runtime worker thread.
//!
//! Per [`design/012-module-apis.md`](../../../design/012-module-apis.md), no function in
//! this crate panics on a normal error path: missing drivers, unsupported platforms,
//! invalid specs and protocol mismatches all surface as `anyhow::Error`.
//!
//! # Validation rules
//!
//! [`validate_spec`] rejects specs the driver could not turn into a sane mode:
//! even width/height inside [`MIN_DIMENSION`]..=[`MAX_DIMENSION`], refresh inside
//! [`MIN_REFRESH_HZ`]..=[`MAX_REFRESH_HZ`], DPI inside [`MIN_DPI`]..=[`MAX_DPI`], and a
//! printable ASCII `name_hint` of at most [`NAME_HINT_MAX_BYTES`] bytes (it travels to
//! the driver as a NUL-terminated ANSI string).
//!
//! # Example
//!
//! ```no_run
//! use dfy_vdisplay::{create_backend, VirtualDisplaySpec};
//!
//! # fn main() -> anyhow::Result<()> {
//! let backend = create_backend()?;
//! if !backend.is_available() {
//!     return Ok(()); // driver not installed on this machine
//! }
//! let spec = VirtualDisplaySpec::new(1920, 1080, 60, 96, "dfy-virtual-1");
//! spec.validate()?;
//! let id = backend.create(&spec)?;
//! assert!(backend.list()?.iter().any(|d| d.id == id));
//! backend.destroy(id)?;
//! # Ok(())
//! # }
//! ```

#![warn(missing_docs)]

use std::fmt;

pub mod stub;

#[cfg(target_os = "linux")]
pub mod linux_xvfb;

#[cfg(windows)]
pub mod win_driver;

/// Smallest accepted display dimension, in pixels, for either axis.
///
/// Below this a virtual display has no useful desktop area and several Windows
/// display paths start rejecting modes.
pub const MIN_DIMENSION: u32 = 16;

/// Largest accepted display dimension, in pixels, for either axis.
///
/// Kept below the IddCx/D3D11 texture limit (16384) so a single swap-chain buffer is
/// always representable.
pub const MAX_DIMENSION: u32 = 16384;

/// Smallest accepted refresh rate, in Hz.
pub const MIN_REFRESH_HZ: u32 = 1;

/// Largest accepted refresh rate, in Hz.
pub const MAX_REFRESH_HZ: u32 = 360;

/// Smallest accepted DPI value (25 % scale of the 96 DPI baseline).
pub const MIN_DPI: u32 = 24;

/// Largest accepted DPI value (500 % scale of the 96 DPI baseline).
pub const MAX_DPI: u32 = 480;

/// Maximum length, in bytes, of [`VirtualDisplaySpec::name_hint`].
///
/// The driver-side ABI carries the hint in a fixed `char[64]` buffer that must stay
/// NUL-terminated, hence 63 usable bytes.
pub const NAME_HINT_MAX_BYTES: usize = 63;

/// Capability flags a backend may report through [`VirtualDisplayBackend::capabilities`].
pub mod capability {
    /// More than one virtual display can exist at the same time.
    pub const MULTI_MONITOR: u32 = 1 << 0;
    /// Resolution / refresh / DPI can be changed on a live display.
    pub const DYNAMIC_MODES: u32 = 1 << 1;
    /// The backend can report monitor arrival / departure / mode-change events.
    pub const HOTPLUG_EVENTS: u32 = 1 << 2;
    /// The backend can expose an HDR-capable virtual monitor.
    pub const HDR: u32 = 1 << 3;
}

/// Human readable name of the running platform, used in error messages.
pub fn platform_name() -> &'static str {
    #[cfg(windows)]
    {
        "windows"
    }
    #[cfg(target_os = "linux")]
    {
        "linux"
    }
    #[cfg(target_os = "macos")]
    {
        "macos"
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        "unknown"
    }
}

/// Whether a real virtual display backend is compiled in for this platform.
///
/// This says nothing about whether the mechanism is *installed* (the Windows driver can
/// be missing); use [`VirtualDisplayBackend::is_available`] for that.
pub const fn is_platform_supported() -> bool {
    cfg!(any(windows, target_os = "linux"))
}

/// Description of a virtual display to create or update.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VirtualDisplaySpec {
    /// Width in pixels; must be even and inside [`MIN_DIMENSION`]..=[`MAX_DIMENSION`].
    pub width: u32,
    /// Height in pixels; must be even and inside [`MIN_DIMENSION`]..=[`MAX_DIMENSION`].
    pub height: u32,
    /// Vertical refresh rate in Hz; must be inside [`MIN_REFRESH_HZ`]..=[`MAX_REFRESH_HZ`].
    pub refresh_hz: u32,
    /// Monitor DPI (96 = 100 % scaling); must be inside [`MIN_DPI`]..=[`MAX_DPI`].
    pub dpi: u32,
    /// Printable ASCII label shown to the driver and in logs; at most
    /// [`NAME_HINT_MAX_BYTES`] bytes. Empty means "let the backend pick a default".
    pub name_hint: String,
}

impl VirtualDisplaySpec {
    /// Builds a spec without validating it; call [`Self::validate`] before using it.
    pub fn new(
        width: u32,
        height: u32,
        refresh_hz: u32,
        dpi: u32,
        name_hint: impl Into<String>,
    ) -> Self {
        Self {
            width,
            height,
            refresh_hz,
            dpi,
            name_hint: name_hint.into(),
        }
    }

    /// A conventional 1920×1080 @ 60 Hz, 96 DPI display with an empty name hint.
    pub fn hd_1080p() -> Self {
        Self::new(1920, 1080, 60, 96, "")
    }

    /// Validates this spec, returning a single error that lists every violation.
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_spec(self).map_err(anyhow::Error::new)
    }
}

impl Default for VirtualDisplaySpec {
    fn default() -> Self {
        Self::hd_1080p()
    }
}

/// All rule violations found by [`validate_spec`].
///
/// The list is collected in one pass so callers see every problem at once instead of
/// fixing them one error at a time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpecViolations {
    /// One human readable sentence per violated rule, in rule evaluation order.
    pub reasons: Vec<String>,
}

impl fmt::Display for SpecViolations {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid virtual display spec: {}",
            self.reasons.join("; ")
        )
    }
}

impl std::error::Error for SpecViolations {}

/// Validates every field of `spec` and reports all violations at once.
pub fn validate_spec(spec: &VirtualDisplaySpec) -> Result<(), SpecViolations> {
    let mut reasons = Vec::new();

    // Width and height: even, so chroma-subsampled encoders (NV12 for H.264/H.265) can
    // consume the frame without cropping, and both inside the supported range.
    for (axis, value) in [("width", spec.width), ("height", spec.height)] {
        if !(MIN_DIMENSION..=MAX_DIMENSION).contains(&value) {
            reasons.push(format!(
                "{axis} {value} outside supported range {MIN_DIMENSION}..={MAX_DIMENSION}"
            ));
        } else if value % 2 != 0 {
            reasons.push(format!(
                "{axis} {value} is odd; even dimensions are required for YUV 4:2:0 encoding"
            ));
        }
    }

    if spec.refresh_hz < MIN_REFRESH_HZ || spec.refresh_hz > MAX_REFRESH_HZ {
        reasons.push(format!(
            "refresh_hz {} outside supported range {MIN_REFRESH_HZ}..={MAX_REFRESH_HZ}",
            spec.refresh_hz
        ));
    }

    if spec.dpi < MIN_DPI || spec.dpi > MAX_DPI {
        reasons.push(format!(
            "dpi {} outside supported range {MIN_DPI}..={MAX_DPI}",
            spec.dpi
        ));
    }

    // The name hint is copied into a NUL-terminated ANSI buffer shared with the driver,
    // so it must be printable ASCII and must fit before the terminator.
    if spec.name_hint.len() > NAME_HINT_MAX_BYTES {
        reasons.push(format!(
            "name_hint is {} bytes, limit is {NAME_HINT_MAX_BYTES}",
            spec.name_hint.len()
        ));
    }
    if let Some(bad) = spec
        .name_hint
        .chars()
        .find(|c| !(c.is_ascii_graphic() || *c == ' '))
    {
        reasons.push(format!(
            "name_hint contains unsupported character {bad:?}; only printable ASCII is allowed"
        ));
    }

    if reasons.is_empty() {
        Ok(())
    } else {
        Err(SpecViolations { reasons })
    }
}

/// Snapshot of one virtual display as reported by its backend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VirtualDisplayInfo {
    /// Backend-assigned id; stable while the display is plugged in.
    ///
    /// Ids are not reused after [`VirtualDisplayBackend::destroy`], and a driver restart
    /// may hand out different ids, so callers must re-[`list`](VirtualDisplayBackend::list)
    /// after a recovery instead of caching these across driver lifetimes.
    pub id: u32,
    /// Current geometry/refresh/DPI of the display.
    pub spec: VirtualDisplaySpec,
    /// Whether the monitor is currently attached to the desktop.
    ///
    /// A display can be known to the driver but detached (created and then plugged out,
    /// or waiting for the OS to enumerate it); attached displays are the ones DXGI can
    /// capture from.
    pub attached: bool,
}

/// One OS-specific way of creating and driving virtual displays.
///
/// Implementations must be usable from several threads, because the session layer
/// creates displays from its control task while the capture task polls state. All
/// methods take `&self`; implementations are expected to serialise device access
/// internally.
pub trait VirtualDisplayBackend: Send + Sync {
    /// Returns every virtual display the driver currently knows about.
    ///
    /// The result includes detached displays; check [`VirtualDisplayInfo::attached`].
    fn list(&self) -> anyhow::Result<Vec<VirtualDisplayInfo>>;

    /// Creates a new virtual display and returns its backend-assigned id.
    ///
    /// The display is attached to the desktop by the time this returns, but Windows may
    /// take a few hundred milliseconds to publish the corresponding DXGI output; capture
    /// code should retry enumeration instead of failing.
    fn create(&self, spec: &VirtualDisplaySpec) -> anyhow::Result<u32>;

    /// Changes geometry/refresh/DPI of an existing display.
    ///
    /// Only the fields of [`VirtualDisplaySpec`] are applied; `name_hint` is ignored on
    /// update because the driver derives the monitor's friendly name from the creation
    /// hint. Must not disturb other displays or the physical monitors.
    ///
    /// Linux prefers `xrandr --fb`; if Xvfb cannot perform the requested live resize, it
    /// recreates only this display's X server under the same id. X11 clients must then
    /// reconnect. Refresh-rate and DPI changes are unsupported on Linux.
    fn update(&self, id: u32, spec: &VirtualDisplaySpec) -> anyhow::Result<()>;

    /// Removes a virtual display and releases its swap chain.
    ///
    /// Destroying an unknown or already destroyed id is an error, not a silent no-op, so
    /// callers notice double-destroy bugs.
    fn destroy(&self, id: u32) -> anyhow::Result<()>;

    /// Whether the backend mechanism is present and usable right now.
    ///
    /// This is a cheap probe meant for UI/status display and for watchdog loops; it must
    /// not panic and must not block for long. On Windows it opens the device interface
    /// and queries the protocol version.
    fn is_available(&self) -> bool;

    /// Bitmask of [`capability`] flags this backend supports.
    fn capabilities(&self) -> u32 {
        0
    }

    /// Returns a backend-specific endpoint that capture/input clients can connect to.
    ///
    /// Linux Xvfb returns its managed `:N` X11 endpoint. Backends whose displays are
    /// enumerated through the operating system, such as DXGI, return `None`.
    fn capture_endpoint(&self, id: u32) -> anyhow::Result<Option<String>> {
        let _ = id;
        Ok(None)
    }
}

/// Creates the backend for the current platform.
///
/// On Windows this returns the IddCx driver control plane. The device is opened lazily,
/// so a machine without the driver still gets `Ok(..)` and can ask
/// [`VirtualDisplayBackend::is_available`]; operations then fail with a descriptive
/// error. On Linux this returns [`linux_xvfb::XvfbBackend`], which manages isolated Xvfb
/// desktops without root or access to the physical display. Other platforms return
/// [`stub::UnsupportedBackend`], whose methods report `Unsupported` and whose availability
/// is `false`.
pub fn create_backend() -> anyhow::Result<Box<dyn VirtualDisplayBackend>> {
    #[cfg(windows)]
    {
        Ok(Box::new(win_driver::WinDriverBackend::new()))
    }
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(linux_xvfb::XvfbBackend::new()?))
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        Ok(Box::new(stub::UnsupportedBackend::new(platform_name())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn violations(spec: &VirtualDisplaySpec) -> Vec<String> {
        match validate_spec(spec) {
            Ok(()) => Vec::new(),
            Err(err) => err.reasons,
        }
    }

    #[test]
    fn spec_default_and_hd_1080p_are_valid() {
        assert!(VirtualDisplaySpec::default().validate().is_ok());
        assert!(VirtualDisplaySpec::hd_1080p().validate().is_ok());
        assert_eq!(
            VirtualDisplaySpec::default(),
            VirtualDisplaySpec::hd_1080p()
        );
    }

    #[test]
    fn odd_dimensions_are_rejected() {
        let mut spec = VirtualDisplaySpec::hd_1080p();
        spec.width = 1921;
        let reasons = violations(&spec);
        assert_eq!(reasons.len(), 1);
        assert!(reasons[0].contains("width 1921 is odd"), "{reasons:?}");

        let mut spec = VirtualDisplaySpec::hd_1080p();
        spec.height = 1079;
        let err = validate_spec(&spec).unwrap_err();
        assert!(err.to_string().contains("height 1079 is odd"));
        assert!(err.to_string().contains("invalid virtual display spec"));
    }

    #[test]
    fn dimensions_outside_range_are_rejected() {
        let mut spec = VirtualDisplaySpec::hd_1080p();
        spec.width = MIN_DIMENSION - 2;
        spec.height = MAX_DIMENSION + 2;
        let reasons = violations(&spec);
        assert_eq!(reasons.len(), 2, "{reasons:?}");
        assert!(reasons[0].contains("outside supported range"));
        assert!(reasons[1].contains("outside supported range"));
    }

    #[test]
    fn refresh_and_dpi_ranges_are_enforced() {
        let mut spec = VirtualDisplaySpec::hd_1080p();
        spec.refresh_hz = 0;
        spec.dpi = MIN_DPI - 1;
        let reasons = violations(&spec);
        assert_eq!(reasons.len(), 2, "{reasons:?}");
        assert!(reasons[0].contains("refresh_hz 0"));
        assert!(reasons[1].contains("dpi 23"));

        let mut spec = VirtualDisplaySpec::hd_1080p();
        spec.refresh_hz = MAX_REFRESH_HZ + 1;
        spec.dpi = MAX_DPI + 1;
        let reasons = violations(&spec);
        assert_eq!(reasons.len(), 2, "{reasons:?}");
    }

    #[test]
    fn name_hint_limits_are_enforced() {
        let mut spec = VirtualDisplaySpec::hd_1080p();
        spec.name_hint = "x".repeat(NAME_HINT_MAX_BYTES);
        assert!(spec.validate().is_ok(), "63 bytes must fit");

        spec.name_hint = "x".repeat(NAME_HINT_MAX_BYTES + 1);
        let reasons = violations(&spec);
        assert_eq!(reasons.len(), 1, "{reasons:?}");
        assert!(reasons[0].contains("limit is 63"));

        spec.name_hint = "虚拟屏".to_string();
        let reasons = violations(&spec);
        assert!(
            reasons.iter().any(|r| r.contains("unsupported character")),
            "{reasons:?}"
        );

        spec.name_hint = "bad\u{0}name".to_string();
        let reasons = violations(&spec);
        assert!(
            reasons.iter().any(|r| r.contains("unsupported character")),
            "{reasons:?}"
        );
    }

    #[test]
    fn every_violation_is_reported_in_one_pass() {
        let spec = VirtualDisplaySpec::new(3, 3, 0, 0, "!");
        let reasons = violations(&spec);
        // width range + height range + refresh + dpi = 4.
        assert_eq!(reasons.len(), 4, "{reasons:?}");
    }

    #[test]
    fn platform_helpers_are_consistent() {
        assert!(!platform_name().is_empty());
        #[cfg(any(windows, target_os = "linux"))]
        assert!(is_platform_supported());
        #[cfg(not(any(windows, target_os = "linux")))]
        assert!(!is_platform_supported());
    }
}
