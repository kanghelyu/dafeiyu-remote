# Windows Release & Inno Setup (0.1.0 / 0.1.1) Build Report (2026-11-27)

## Scope & Environment
- Host: Windows 11 x64 (`x86_64-pc-windows-gnu`, `rustc 1.100.0`, Inno Setup `6.7.3`)
- Changes verified:
  - `packaging/windows/build-installer.sh`: auto-detects per-user `%LOCALAPPDATA%\Programs\Inno Setup 6\ISCC.exe` and optionally bundles `dfy-manager.exe` (`/DHasManager=1`).
  - `packaging/windows/dfy.iss`: updated publisher/support URLs to `kanghelyu/dafeiyu-remote`, added conditional `dfy-manager.exe` + Start Menu / desktop icons (`dfy.ico`).

## Build & Binary Artifacts (SHA-256)
Command:
```powershell
cargo build --release --locked -p dfy-cli -p dfy-relay -p dfy-manager
bash packaging/windows/build-installer.sh --binary-dir ... --version 0.1.0 --output-dir ...
bash packaging/windows/build-installer.sh --binary-dir ... --version 0.1.1 --output-dir ...
```

| Artifact | SHA-256 |
| --- | --- |
| `dfy.exe` (Release) | `E1198C088DF20AFFE3A85B31662DF2148C0E89A4570F98D13AFB62C7D9459A7F` |
| `dfy-relay.exe` (Release) | `7E813BEF2CD3EB03C0B480B37E4B493058C84F4BDB2ACA87B6B6F7CC374CFF7E` |
| `dfy-manager.exe` (Release) | `418FD0465C3A43E4458643CCBC1725CB62CE52944A4DE0F5F6173339B9A5E4F0` |
| `dafeiyu-0.1.0-windows-x86_64-setup.exe` | `19E649FC17462DCAA1E44C5032E67B279481F539903CCA1233141905D5758569` |
| `dafeiyu-0.1.1-windows-x86_64-setup.exe` | `87347CE815E3CD17138DC6BFB1074BB2C36C1518F8433552B3E954902CC009A3` |

## Runtime / Upgrade Execution Boundary
- `dfy-relay.exe --version` executed cleanly on Windows (`dfy-relay 0.1.0`, exit 0).
- Non-GUI Rust crates (`dfy-proto`, `dfy-path`, `dfy-relay-profiles`, `dfy-relay`, `dfy-transport`, `dfy-p2p`, `dfy-files`, `dfy-audio`) passed `cargo test` on Windows.
- **Sandbox Desktop Boundary**: In this shell's isolated sandbox desktop (`Desktop: exebox-...`), binaries statically importing `user32.dll`/`ole32.dll` (`dfy.exe`, `dfy-manager.exe`, `dfy-ui` tests) exit with `0xc0000142` (`STATUS_DLL_INIT_FAILED`) and GUI Inno Setup `Setup.exe` `/VERYSILENT` hung without creating files (cancelled & cleaned up).
- Therefore, `0.1.0 -> 0.1.1 -> unins000.exe` interactive/silent upgrade execution and code-signing remain **unverified** on an interactive Windows desktop (see `STATUS.md` row 36 for prior 0.1.0 silent install/uninstall pass).
