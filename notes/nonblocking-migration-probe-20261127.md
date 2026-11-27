# Non-Blocking Path Migration & 64B vs 1200B Probe Calibration (2026-11-27)

## Changes
- `crates/dfy-cli/src/real.rs`: refactored `Action::RequestProbe` and `Action::SwitchTo` out of inline `.await` in `tokio::select!` into background `tokio::spawn` tasks (`pending_probe`, `pending_migration`), preventing handshake/probe RTT from stalling video decode, audio jitter playback, heartbeats, or input.
- `crates/dfy-cli/src/main.rs` & `real.rs`: added `--probe-bytes <N>` (clamped `64..=1200`, default `1200`) to `View` / `ViewOptions`.

## Test 1: Non-Blocking Migration + Audio Continuity (Debian 13 Guest)
- Command: local `dfy-relay` (`127.0.0.1:21127`) + `dfy agent --auto-path --virtual-displays 1 --virtual-width 640 --virtual-height 480 --audio-out --audio-test-tone` + `dfy view --auto-path --force-switch-once relay --audio-in --seconds 10`
- Results:
  - `会话已迁移到中继（epoch=1, 耗时 602ms，帧=#13）`
  - `迁移时清空旧音频序号的缓冲 queued=0` (no backlog accumulated during handshake)
  - Video: `DATAGRAM = 1165（1.14 MiB），重组帧 = 193，解码帧 = 193，关键帧 = 193，解析失败 = 0`
  - Audio: `DATAGRAM = 200，播放帧 = 200，最后序号 = 182，丢包 = 0，淘汰 = 0，乱序 = 0，重复 = 0，峰值深度 = 4`
  - Migration: `路径迁移：次数 = 1，恢复确认 = true，最后帧 = #208/208`

## Test 2: 64B vs 1200B Active Path Probe Calibration (MTU-Drop Proxy)
- Proxy: `probe_drop_proxy.py` on `127.0.0.1:21336 -> 21326` allows initial 6 QUIC handshake packets per client port, then drops all UDP packets `> 400B`.
- `--probe-bytes 64`:
  - 5 consecutive probes reported `rtt_ms=3.18..5.23`, `loss_ppm=0`.
  - Falsely concluded Direct was healthy and switched to broken Direct: `会话已迁移到直连（epoch=1, 耗时 608ms，帧=#143）`, `路径迁移：次数 = 1`.
- `--probe-bytes 1200`:
  - Detected MTU-sized packet loss: `loss_ppm=285714` (28.6%) and `loss_ppm=250000` (25.0%), `rtt_ms=4.69..11.08`.
  - Refused to switch back to broken Direct: `路径迁移：次数 = 0，恢复确认 = false`, staying on healthy Relay.
- Conclusion: 1200B probing (the default) is verified necessary to detect MTU/large-packet path blackholes that 64B probes miss.
