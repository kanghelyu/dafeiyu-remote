# Linux DEB/RPM & systemd Service Acceptance (2026-11-27)

## Environment
- Guest: Debian 13 (`Linux dfy-linux-lab 6.12.118+deb13-cloud-amd64 x86_64`)
- Tools: `dpkg-deb`, `rpmbuild` / `rpm` 4.20.1, `systemd`, `curl`

## Packaging Fix Caught & Verified
- `packaging/linux/build-packages.sh` `build_rpm()` previously lacked an `%install` section in `dafeiyu.spec`, causing RPM 4.20 `rpmbuild -bb` to fail with `File not found: .../BUILDROOT/usr/bin/dfy`.
- Added `%install` (`mkdir -p %{buildroot} && cp -a "$STAGE/usr" "$STAGE/etc" %{buildroot}/`) and rebuilt both `.deb` and `.rpm` with `--format all`.

## Built Artifacts & Verification
| Artifact | SHA-256 | Verification |
| --- | --- | --- |
| `dafeiyu_0.1.0_amd64.deb` | `e7e803d41a1d2e8eea8ee401c6acb9ed23994d14eca3a8b49a104b9401e84fa3` | `verify-deb.sh`: `DEB verification PASS` |
| `dafeiyu-0.1.0-1.x86_64.rpm` | `2c7db73f954db9260a44f7438292afdb2208b403b3691025cbec48dc7bcebb01` | `rpm -qpi`, `--requires`, `--scripts`, `--dump` PASS (`License: LicenseRef-PolyForm-Noncommercial-1.0.0`, all 10 payload files + `%config(noreplace)`) |

## Real `systemd` `dfy-relay.service` Lifecycle
1. `sudo -n dpkg -i dafeiyu_0.1.0_amd64.deb`:
   - `systemctl is-enabled dfy-relay` -> `disabled`
   - `systemctl is-active dfy-relay` -> `inactive`
2. `sudo -n dfy-relay config init --out /etc/dfy-relay/relay.toml`, `chown root:dfy-relay`, `chmod 0640`:
   - `sudo -n dfy-relay config check --config /etc/dfy-relay/relay.toml` -> `配置校验通过`
3. `sudo -n systemctl start dfy-relay`:
   - `systemctl is-active dfy-relay` -> `active`
   - `curl -fsS http://127.0.0.1:21119/healthz` -> `ok`
   - `curl -fsS http://127.0.0.1:21119/status` -> `{"ok":true,"version":"0.1.0","uptime_seconds":1,...,"bind":"0.0.0.0:21117","tcp_bind":"0.0.0.0:21118","admin":"127.0.0.1:21119","no_log_ips":true}`
4. Cleanup: `systemctl stop dfy-relay` (`inactive`), removed test `/etc/dfy-relay/relay.toml`, `dpkg -P dafeiyu` (`no packages found matching dafeiyu`).

## Remaining Boundaries
- RPM was built and inspected (`rpm -qpi/--requires/--scripts/--dump`) on Debian; native RPM installation on a Fedora/RHEL host and `aarch64` builds remain unverified locally.
