# CI/Release Fixes, SBOM Audit, Window Icons, 8-Metric HUD & Manager Enhancements (2026-11-27)

## 1. Cross-Platform CI & Release Workflow Fixes
- `crates/dfy-android/src/agent.rs`: added `#[cfg_attr(not(test), allow(dead_code))]` on `AgentHost::bind` / `bind_with_identity` (fixes `windows-latest` clippy).
- `crates/dfy-vdisplay/src/linux_xvfb.rs`: removed redundant `as u32` on `getuid()` (fixes `ubuntu-latest` clippy).
- `crates/dfy-manager/src/main.rs`: added `target_os = "macos"` font candidates and `#[allow(unused_mut)]` (fixes `macos-latest` clippy).
- `crates/dfy-cli/src/real.rs`: unified `tracing::warn` import and gated `AtomicBool`/`Ordering`/`Arc`/`PERM_AUDIO_OUT` to `cfg(any(windows, target_os = "linux"))` (fixes `macos-latest` clippy).
- `.github/workflows/release.yml`: migrated `macos-14` -> `macos-15` and accepted SemVer prerelease/build tags (e.g., `v0.1.0-source-available`) matching workspace version `0.1.0`.

## 2. CycloneDX SBOM & Dependency License Audit
- Added `packaging/generate_sbom.py` and wired into `.github/workflows/ci.yml` and `.github/workflows/release.yml`.
- Ran `python packaging/generate_sbom.py --check --output-sbom .tmp/sbom/SBOM-cyclonedx.json --output-notices .tmp/sbom/THIRD-PARTY-NOTICES.txt`:
  - `SBOM AUDIT PASS: 18 workspace crates (LicenseRef-PolyForm-Noncommercial-1.0.0), 354 third-party dependencies audited`.

## 3. Window Icons, 8-Metric Viewer HUD & `dfy-manager`
- `crates/dfy-ui`: added `png 0.18` window icon decoding (`assets/icon-512.png`), `ConnectionStats` + `RenderStats::title_with_connection`, `PresenterHandle::update_stats`, wired 500ms live metrics (resolution, FPS, bitrate, RTT, loss, decode/render latency, path, relay) from `dfy-cli/src/real.rs`.
- `crates/dfy-manager`: added `assets/icon-512.png` window icon, switched `ControlFlow::Poll` -> `ControlFlow::Wait`, allowed empty-config startup, added `S` (cycle selection strategy), `E` (redacted export), and `[4] Devices` tab (`F` favorite toggle, `Enter` non-blocking `dfy view` launch).

## 4. Verification Summary
- Windows: `cargo fmt --all -- --check` PASS; `cargo clippy --workspace --all-targets -- -D warnings` PASS; `packaging/generate_sbom.py --check` PASS; `packaging/tests/smoke.py` PASS; non-GUI crates (`dfy-proto`, `dfy-path`, `dfy-relay-profiles`, `dfy-relay`, `dfy-transport`, `dfy-p2p`, `dfy-files`, `dfy-audio`) `cargo test` PASS.
- Debian 13 Linux guest: `cargo test --locked --config profile.test.package.argon2.opt-level=3 -p dfy-ui -p dfy-manager -p dfy-cli -p dfy-audio -p dfy-path -p dfy-proto -p dfy-relay-profiles -p dfy-vdisplay -p dfy-session` ALL PASS (`dfy-ui` 62+3, `dfy-manager` 9, `dfy-cli` 45+2 ignored, `dfy-session` 74+1, `dfy-path` 57+2, `dfy-proto` 22+1, `dfy-relay-profiles` 5, `dfy-audio` 7, `dfy-vdisplay` 22+1 ignored); `dfy selftest` Release PASS (`120` datagrams, `RTT = 4.395 ms`).
