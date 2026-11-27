// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (C) 2025 Dafeiyu Remote Project

//! Reference implementation that reports "this platform has no virtual display
//! backend yet".
//!
//! It exists rather than a bare `Err` from [`create_backend`](crate::create_backend)
//! because the trait already models availability: callers that need to run on every
//! platform can keep one code path and gate features on
//! [`VirtualDisplayBackend::is_available`](crate::VirtualDisplayBackend::is_available).
//! Every operation fails with an `Unsupported` error so nothing silently succeeds.
//!
//! It is compiled on **all** platforms (including Windows) so its behaviour is covered
//! by unit tests in the normal `cargo test -p dfy-vdisplay` run.
//!
//! # Where the real implementations will live
//!
//! * Linux — the production backend is `crate::linux_xvfb`: one isolated,
//!   no-root Xvfb server per desktop. It is available on Linux targets only; evdi/DRM
//!   integration with a Wayland compositor remains future work.
//! * macOS — `CGVirtualDisplay` in CoreGraphics, a private (undocumented, App Store
//!   rejected) API; a future `src/mac_cgvirtualdisplay.rs` will bind it through
//!   `objc2`/`CoreGraphics` and keep the same trait shape.
//!
//! macOS is not implemented yet; see `design/020-virtual-display.md` for the milestone
//! plan and licence constraints. This stub remains useful on every platform for callers
//! that explicitly need a feature-gated unsupported backend.

use anyhow::{anyhow, Result};

use crate::{
    capability, platform_name, VirtualDisplayBackend, VirtualDisplayInfo, VirtualDisplaySpec,
};

/// Backend that rejects every operation with an `Unsupported` error.
#[derive(Clone, Copy, Debug)]
pub struct UnsupportedBackend {
    platform: &'static str,
}

impl UnsupportedBackend {
    /// Creates the stub for `platform` (a short name such as `"linux"`).
    pub fn new(platform: &'static str) -> Self {
        Self { platform }
    }

    /// Creates the stub for the running platform.
    pub fn for_current_platform() -> Self {
        Self::new(platform_name())
    }

    /// Platform name reported in error messages.
    pub fn platform(&self) -> &'static str {
        self.platform
    }

    fn unsupported(&self, operation: &str) -> anyhow::Error {
        anyhow!(
            "virtual display backend is not implemented on {} yet \
             (operation: {operation}); see design/020-virtual-display.md for the plan",
            self.platform
        )
    }
}

impl VirtualDisplayBackend for UnsupportedBackend {
    fn list(&self) -> Result<Vec<VirtualDisplayInfo>> {
        Err(self.unsupported("list"))
    }

    fn create(&self, _spec: &VirtualDisplaySpec) -> Result<u32> {
        Err(self.unsupported("create"))
    }

    fn update(&self, _id: u32, _spec: &VirtualDisplaySpec) -> Result<()> {
        Err(self.unsupported("update"))
    }

    fn destroy(&self, _id: u32) -> Result<()> {
        Err(self.unsupported("destroy"))
    }

    fn is_available(&self) -> bool {
        false
    }

    fn capabilities(&self) -> u32 {
        // Nothing to offer: not even multi-monitor, since no display can be created.
        let _ = capability::MULTI_MONITOR; // documents that the stub advertises nothing
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend() -> UnsupportedBackend {
        UnsupportedBackend::new("linux")
    }

    /// The stub must be usable through the trait object, exactly like the real backend.
    fn as_trait_object(backend: &dyn VirtualDisplayBackend) {
        assert!(!backend.is_available());
        assert_eq!(backend.capabilities(), 0);

        let err = backend.list().unwrap_err();
        let message = err.to_string();
        assert!(message.contains("not implemented on linux"), "{message}");
        assert!(
            message.contains("design/020-virtual-display.md"),
            "{message}"
        );

        let spec = VirtualDisplaySpec::hd_1080p();
        assert!(backend.create(&spec).is_err());
        assert!(backend.update(1, &spec).is_err());
        assert!(backend.destroy(1).is_err());
    }

    #[test]
    fn every_operation_reports_unsupported() {
        as_trait_object(&backend());
    }

    #[test]
    fn stub_is_not_available_and_does_not_claim_capabilities() {
        let backend = backend();
        assert!(!backend.is_available());
        assert_eq!(
            <UnsupportedBackend as VirtualDisplayBackend>::capabilities(&backend),
            0
        );
        assert_eq!(backend.platform(), "linux");
    }

    #[test]
    fn errors_name_the_failing_operation() {
        let backend = backend();
        for operation in ["list", "create", "update", "destroy"] {
            let err = match operation {
                "list" => backend.list().err(),
                "create" => backend.create(&VirtualDisplaySpec::hd_1080p()).err(),
                "update" => backend.update(3, &VirtualDisplaySpec::hd_1080p()).err(),
                _ => backend.destroy(3).err(),
            }
            .expect("operation must fail");
            assert!(
                err.to_string().contains(&format!("operation: {operation}")),
                "{operation}: {err}"
            );
        }
        // `for_current_platform` must not panic even on exotic targets.
        let _ = UnsupportedBackend::for_current_platform().platform();
    }
}
