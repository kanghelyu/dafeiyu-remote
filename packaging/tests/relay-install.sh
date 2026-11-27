#!/usr/bin/env bash
# SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
# Full installer smoke test with mocked root/systemd/network commands and real tar/checksums.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TEST_ROOT="$(mktemp -d)"
trap 'rm -rf -- "$TEST_ROOT"' EXIT
MOCK_BIN="$TEST_ROOT/mock-bin"
mkdir -p "$MOCK_BIN" "$TEST_ROOT/fixture"
printf 'ID=test\nPRETTY_NAME="Packaging test Linux"\n' > "$TEST_ROOT/os-release"
export DFY_OS_RELEASE_FILE="$TEST_ROOT/os-release"

fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf 'PASS %s\n' "$*"; }

cat > "$TEST_ROOT/fixture/dfy-relay" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [ "$1 $2" = "config init" ]; then
  shift 2
  out=""; bind="0.0.0.0:21117"; tcp="0.0.0.0:21118"; admin="127.0.0.1:21119"; no_tcp=0
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --out) out="$2"; shift 2 ;;
      --bind) bind="$2"; shift 2 ;;
      --tcp-bind) tcp="$2"; shift 2 ;;
      --admin) admin="$2"; shift 2 ;;
      --no-tcp) no_tcp=1; shift ;;
      *) exit 2 ;;
    esac
  done
  [ "$no_tcp" = 0 ] || tcp=""
  key="$(python3 -c 'import base64,secrets; print("base64:"+base64.b64encode(secrets.token_bytes(32)).decode())')"
  cat > "$out" <<TOML
bind = "$bind"
tcp_bind = "$tcp"
admin_addr = "$admin"
relay_key = "$key"
max_sessions = 1024
per_tag_mbps = 200
tag_ttl_hours = 48
TOML
elif [ "$1 $2" = "config check" ]; then
  [ "${MOCK_CONFIG_CHECK_FAIL:-0}" != 1 ] || exit 1
  python3 - "$4" <<'PY'
import ipaddress, sys, tomllib
with open(sys.argv[1], 'rb') as stream:
    data = tomllib.load(stream)
assert len(data['relay_key'].removeprefix('base64:')) == 44
assert ipaddress.ip_address(data['admin_addr'].rsplit(':',1)[0]).is_loopback
PY
else
  exit 2
fi
EOF
chmod +x "$TEST_ROOT/fixture/dfy-relay"
tar -C "$TEST_ROOT/fixture" -czf "$TEST_ROOT/relay.tar.gz" dfy-relay
ARCHIVE_HASH="$(sha256sum "$TEST_ROOT/relay.tar.gz" | awk '{print $1}')"
printf '%s  dfy-relay-linux-x86_64.tar.gz\n' "$ARCHIVE_HASH" > "$TEST_ROOT/relay.sha256"
cp "$TEST_ROOT/relay.sha256" "$TEST_ROOT/SHA256SUMS.txt"

cat > "$MOCK_BIN/id" <<'EOF'
#!/usr/bin/env bash
printf '0\n'
EOF
# MSYS on Windows cannot chmod some temporary directories; real Linux runs unchanged.
cat > "$MOCK_BIN/install" <<'EOF'
#!/usr/bin/env bash
set -eu
if [ "${1:-}" = -d ]; then
  directory="${@: -1}"
  mkdir -p "$directory"
  exit 0
fi
exec /usr/bin/install "$@"
EOF
cat > "$MOCK_BIN/uname" <<'EOF'
#!/usr/bin/env bash
printf 'x86_64\n'
EOF
cat > "$MOCK_BIN/getent" <<'EOF'
#!/usr/bin/env bash
exit 1
EOF
for command in groupadd useradd addgroup adduser chgrp sleep; do
  cat > "$MOCK_BIN/$command" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
done
cat > "$MOCK_BIN/systemctl" <<'EOF'
#!/usr/bin/env bash
set -eu
printf '%s\n' "$*" >> "$MOCK_SYSTEMCTL_LOG"
case "$1" in
  is-active) [ -f "$MOCK_ACTIVE" ] ;;
  restart) touch "$MOCK_ACTIVE" ;;
  stop) rm -f "$MOCK_ACTIVE" ;;
  *) exit 0 ;;
esac
EOF
cat > "$MOCK_BIN/curl" <<'EOF'
#!/usr/bin/env bash
set -eu
out=""; url=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o) out="$2"; shift 2 ;;
    --*) shift ;;
    *) url="$1"; shift ;;
  esac
done
case "$url" in
  */healthz) [ "${MOCK_HEALTH_OK:-1}" = 1 ] && printf 'ok\n' || exit 22 ;;
  *.tar.gz.sha256) [ "${MOCK_CHECKSUM_MODE:-sidecar}" = sidecar ] || exit 22; cp "$MOCK_SHA256" "$out" ;;
  */SHA256SUMS.txt) [ "${MOCK_CHECKSUM_MODE:-sidecar}" = global ] || exit 22; cp "$MOCK_GLOBAL_SHA256" "$out" ;;
  *.tar.gz) cp "$MOCK_ARCHIVE" "$out" ;;
  *) printf 'unexpected mock URL: %s\n' "$url" >&2; exit 22 ;;
esac
EOF
chmod +x "$MOCK_BIN"/*
export PATH="$MOCK_BIN:$PATH"
export MOCK_ARCHIVE="$TEST_ROOT/relay.tar.gz"
export MOCK_SHA256="$TEST_ROOT/relay.sha256"
export MOCK_GLOBAL_SHA256="$TEST_ROOT/SHA256SUMS.txt"
export MOCK_SYSTEMCTL_LOG="$TEST_ROOT/systemctl.log"
export MOCK_ACTIVE="$TEST_ROOT/active"
export DFY_NO_FIREWALL=1 DFY_HEALTH_RETRIES=1 DFY_HEALTH_INTERVAL=0
export DFY_RELEASE_BASE_URL=https://example.invalid/releases/download/v1.2.3

INSTALL_ROOT="$TEST_ROOT/install"
export DFY_PREFIX="$INSTALL_ROOT/bin" DFY_CONFIG_DIR="$INSTALL_ROOT/etc" DFY_SYSTEMD_DIR="$INSTALL_ROOT/systemd"
unset DFY_RELAY_PORT DFY_RELAY_TCP_PORT DFY_ADMIN_ADDR DFY_MAX_SESSIONS DFY_PER_TAG_MBPS DFY_TAG_TTL_HOURS || true

if bash "$ROOT/deploy/install-relay.sh" --relay-port nope >"$TEST_ROOT/invalid.log" 2>&1; then
  fail "invalid port succeeded"
fi
[ ! -e "$DFY_PREFIX" ] || fail "invalid arguments caused side effects"
if bash "$ROOT/deploy/install-relay.sh" --not-an-option >"$TEST_ROOT/unknown.log" 2>&1; then
  fail "unknown argument succeeded"
fi
if DFY_ADMIN_ADDR=0.0.0.0:21119 bash "$ROOT/deploy/install-relay.sh" >"$TEST_ROOT/admin.log" 2>&1; then
  fail "non-loopback admin succeeded"
fi
if DFY_ADMIN_ADDR=127.999.1.1:21119 bash "$ROOT/deploy/install-relay.sh" >"$TEST_ROOT/octet.log" 2>&1; then
  fail "invalid admin IPv4 octet succeeded"
fi
PURGE_ROOT="$TEST_ROOT/unsafe-purge"
if DFY_PREFIX="$PURGE_ROOT/bin" DFY_CONFIG_DIR="$PURGE_ROOT/etc" DFY_SYSTEMD_DIR="$PURGE_ROOT/systemd" DFY_PURGE_CONFIG=1 \
  bash "$ROOT/deploy/install-relay.sh" --uninstall >"$TEST_ROOT/purge.log" 2>&1; then
  fail "unsafe config purge succeeded"
fi
[ ! -e "$PURGE_ROOT" ] || fail "unsafe purge validation caused side effects"
INJECT_ROOT="$TEST_ROOT/path-injection"
if DFY_PREFIX="$INJECT_ROOT/bin%n" DFY_CONFIG_DIR="$INJECT_ROOT/etc" DFY_SYSTEMD_DIR="$INJECT_ROOT/systemd" \
  bash "$ROOT/deploy/install-relay.sh" >"$TEST_ROOT/injection.log" 2>&1; then
  fail "systemd path specifier injection succeeded"
fi
for unsafe_path in / "$INJECT_ROOT/bin/../escape" "$INJECT_ROOT//bin" "$INJECT_ROOT/bin/"; do
  if DFY_PREFIX="$unsafe_path" DFY_CONFIG_DIR="$INJECT_ROOT/etc" DFY_SYSTEMD_DIR="$INJECT_ROOT/systemd" \
    bash "$ROOT/deploy/install-relay.sh" >"$TEST_ROOT/unsafe-path.log" 2>&1; then
    fail "non-canonical install path succeeded: $unsafe_path"
  fi
done
[ ! -e "$INJECT_ROOT" ] || fail "path validation caused side effects"
pass "argument, systemd specifier and non-canonical path validation precede side effects"

NO_CHECKSUM_ROOT="$TEST_ROOT/no-checksum"
if MOCK_CHECKSUM_MODE=none DFY_PREFIX="$NO_CHECKSUM_ROOT/bin" DFY_CONFIG_DIR="$NO_CHECKSUM_ROOT/etc" DFY_SYSTEMD_DIR="$NO_CHECKSUM_ROOT/systemd" \
  bash "$ROOT/deploy/install-relay.sh" --version v1.2.3 >"$TEST_ROOT/no-checksum.log" 2>&1; then
  fail "missing checksum succeeded by default"
fi
[ ! -e "$NO_CHECKSUM_ROOT/bin/dfy-relay" ] || fail "unverified archive installed a binary"
grep -q 'no SHA256SUMS/sidecar available' "$TEST_ROOT/no-checksum.log" || fail "missing checksum failure was not explicit"
pass "missing checksum is rejected by default"

bash "$ROOT/deploy/install-relay.sh" --version v1.2.3 >"$TEST_ROOT/first.log" 2>&1 || {
  cat "$TEST_ROOT/first.log" >&2; fail "first install";
}
[ -x "$DFY_PREFIX/dfy-relay" ] || fail "binary not installed"
[ -f "$DFY_SYSTEMD_DIR/dfy-relay.service" ] || fail "unit not installed"
[ -f "$MOCK_ACTIVE" ] || fail "service not started"
grep -q 'NoNewPrivileges=true' "$DFY_SYSTEMD_DIR/dfy-relay.service" || fail "unit hardening missing"
grep -q 'SHA-256 verified' "$TEST_ROOT/first.log" || fail "checksum not verified"
python3 - "$DFY_CONFIG_DIR/relay.toml" <<'PY'
import sys,tomllib
with open(sys.argv[1],'rb') as stream: data=tomllib.load(stream)
assert data['admin_addr']=='127.0.0.1:21119'
assert data['max_sessions']==1024 and data['per_tag_mbps']==200 and data['tag_ttl_hours']==48
PY
pass "checked download, atomic binary install, config generation, hardened unit and health check"

GLOBAL_ROOT="$TEST_ROOT/global-checksum"
MOCK_CHECKSUM_MODE=global DFY_PREFIX="$GLOBAL_ROOT/bin" DFY_CONFIG_DIR="$GLOBAL_ROOT/etc" DFY_SYSTEMD_DIR="$GLOBAL_ROOT/systemd" \
  bash "$ROOT/deploy/install-relay.sh" --version v1.2.3-rc.1+build.5 >"$TEST_ROOT/global.log" 2>&1 || {
    cat "$TEST_ROOT/global.log" >&2; fail "release-wide SHA256SUMS fallback";
  }
[ -x "$GLOBAL_ROOT/bin/dfy-relay" ] || fail "global checksum fallback did not install"
grep -q 'SHA-256 verified' "$TEST_ROOT/global.log" || fail "global checksum was not verified"
pass "release-wide SHA256SUMS.txt fallback verifies the published artifact name"

python3 - "$DFY_CONFIG_DIR/relay.toml" <<'PY'
from pathlib import Path
import re, sys
path = Path(sys.argv[1])
text = re.sub(r'(?m)^(admin_addr|relay_key) = "([^"]*)"$', r"\1 = '\2'", path.read_text())
path.write_text(text)
PY
printf '\nlog = "debug"\ncustom_value = "preserve # this"\n[extra]\nbind = "nested-bind"\nmax_sessions = 77\n' >> "$DFY_CONFIG_DIR/relay.toml"
before="$(sha256sum "$DFY_CONFIG_DIR/relay.toml" | awk '{print $1}')"
bash "$ROOT/deploy/install-relay.sh" --version v1.2.3 >"$TEST_ROOT/second.log" 2>&1 || {
  cat "$TEST_ROOT/second.log" >&2; fail "idempotent upgrade";
}
after="$(sha256sum "$DFY_CONFIG_DIR/relay.toml" | awk '{print $1}')"
[ "$before" = "$after" ] || fail "upgrade without overrides changed config"
grep -q 'Admin: http://127.0.0.1:21119/status' "$TEST_ROOT/second.log" || fail "literal TOML admin_addr was not read"
grep -q 'Client relay_key: base64:' "$TEST_ROOT/second.log" || fail "literal TOML relay_key was not read"
bash "$ROOT/deploy/install-relay.sh" --version v1.2.3 --max-sessions 2048 >"$TEST_ROOT/override.log" 2>&1 || {
  cat "$TEST_ROOT/override.log" >&2; fail "explicit override";
}
python3 - "$DFY_CONFIG_DIR/relay.toml" <<'PY'
import sys,tomllib
with open(sys.argv[1],'rb') as stream: data=tomllib.load(stream)
assert data['max_sessions']==2048
assert data['per_tag_mbps']==200 and data['tag_ttl_hours']==48
assert data['log']=='debug' and data['custom_value']=='preserve # this'
assert data['extra']=={'bind':'nested-bind','max_sessions':77}
PY
pass "idempotent upgrade, literal TOML strings and field-specific override preserve unrelated values"

BAD_ROOT="$TEST_ROOT/bad-checksum"
cp "$MOCK_ARCHIVE" "$BAD_ROOT.tar.gz"
printf 'corrupt\n' >> "$BAD_ROOT.tar.gz"
if MOCK_ARCHIVE="$BAD_ROOT.tar.gz" DFY_PREFIX="$BAD_ROOT/bin" DFY_CONFIG_DIR="$BAD_ROOT/etc" DFY_SYSTEMD_DIR="$BAD_ROOT/systemd" \
  bash "$ROOT/deploy/install-relay.sh" --version v1.2.3 >"$TEST_ROOT/bad.log" 2>&1; then
  fail "checksum mismatch succeeded"
fi
[ ! -e "$BAD_ROOT/bin/dfy-relay" ] || fail "checksum mismatch installed a binary"
grep -q 'SHA-256 mismatch' "$TEST_ROOT/bad.log" || fail "checksum failure was not explicit"
pass "checksum mismatch prevents installation"

python3 - "$TEST_ROOT/duplicate.tar.gz" <<'PY'
from pathlib import Path
import io, sys, tarfile
with tarfile.open(sys.argv[1], "w:gz") as bundle:
    data = b"#!/bin/sh\nexit 0\n"
    member = tarfile.TarInfo("dfy-relay")
    member.size, member.mode = len(data), 0o755
    bundle.addfile(member, io.BytesIO(data))
    link = tarfile.TarInfo("dfy-relay")
    link.type, link.linkname = tarfile.SYMTYPE, "/bin/false"
    bundle.addfile(link)
PY
DUPLICATE_HASH="$(sha256sum "$TEST_ROOT/duplicate.tar.gz" | awk '{print $1}')"
printf '%s  dfy-relay-linux-x86_64.tar.gz\n' "$DUPLICATE_HASH" > "$TEST_ROOT/duplicate.sha256"
DUPLICATE_ROOT="$TEST_ROOT/duplicate-member"
if MOCK_ARCHIVE="$TEST_ROOT/duplicate.tar.gz" MOCK_SHA256="$TEST_ROOT/duplicate.sha256" \
  DFY_PREFIX="$DUPLICATE_ROOT/bin" DFY_CONFIG_DIR="$DUPLICATE_ROOT/etc" DFY_SYSTEMD_DIR="$DUPLICATE_ROOT/systemd" \
  bash "$ROOT/deploy/install-relay.sh" --version v1.2.3 >"$TEST_ROOT/duplicate.log" 2>&1; then
  fail "duplicate archive member succeeded"
fi
[ ! -e "$DUPLICATE_ROOT/bin/dfy-relay" ] || fail "duplicate archive member installed a binary"
grep -q 'exactly one dfy-relay' "$TEST_ROOT/duplicate.log" || fail "duplicate archive member was not rejected explicitly"
pass "duplicate regular/symlink archive members are rejected"

HEALTH_ROOT="$TEST_ROOT/bad-health"
rm -f "$MOCK_ACTIVE"
if MOCK_HEALTH_OK=0 DFY_PREFIX="$HEALTH_ROOT/bin" DFY_CONFIG_DIR="$HEALTH_ROOT/etc" DFY_SYSTEMD_DIR="$HEALTH_ROOT/systemd" \
  bash "$ROOT/deploy/install-relay.sh" --version v1.2.3 >"$TEST_ROOT/health.log" 2>&1; then
  fail "health failure returned zero"
fi
[ ! -e "$HEALTH_ROOT/bin/dfy-relay" ] || fail "failed first install was not rolled back"
[ ! -e "$HEALTH_ROOT/etc/relay.toml" ] || fail "failed first install left a new configuration"
[ ! -e "$HEALTH_ROOT/systemd/dfy-relay.service" ] || fail "failed first install left an enabled unit"
[ ! -e "$MOCK_ACTIVE" ] || fail "failed first install left the service active"
grep -q 'health check failed' "$TEST_ROOT/health.log" || fail "health failure was not explicit"
pass "health check failure is nonzero and rolls back a first install"

touch "$MOCK_ACTIVE"
before_config="$(sha256sum "$DFY_CONFIG_DIR/relay.toml" | awk '{print $1}')"
before_unit="$(sha256sum "$DFY_SYSTEMD_DIR/dfy-relay.service" | awk '{print $1}')"
before_binary="$(sha256sum "$DFY_PREFIX/dfy-relay" | awk '{print $1}')"
if MOCK_HEALTH_OK=0 bash "$ROOT/deploy/install-relay.sh" --version v1.2.3 --max-sessions 4096 >"$TEST_ROOT/upgrade-health.log" 2>&1; then
  fail "failed upgrade health check succeeded"
fi
[ "$before_config" = "$(sha256sum "$DFY_CONFIG_DIR/relay.toml" | awk '{print $1}')" ] || fail "failed upgrade did not restore config"
[ "$before_unit" = "$(sha256sum "$DFY_SYSTEMD_DIR/dfy-relay.service" | awk '{print $1}')" ] || fail "failed upgrade did not restore unit"
[ "$before_binary" = "$(sha256sum "$DFY_PREFIX/dfy-relay" | awk '{print $1}')" ] || fail "failed upgrade did not restore binary"
if [ ! -e "$MOCK_ACTIVE" ]; then
  cat "$TEST_ROOT/upgrade-health.log" >&2
  cat "$MOCK_SYSTEMCTL_LOG" >&2
  fail "failed upgrade did not restore active service"
fi
pass "failed upgrade restores binary, configuration, unit and prior active state"

if MOCK_CONFIG_CHECK_FAIL=1 bash "$ROOT/deploy/install-relay.sh" --version v1.2.3 --max-sessions 8192 >"$TEST_ROOT/config-failure.log" 2>&1; then
  fail "configuration validation failure succeeded"
fi
[ "$before_config" = "$(sha256sum "$DFY_CONFIG_DIR/relay.toml" | awk '{print $1}')" ] || fail "config failure did not restore config"
[ "$before_unit" = "$(sha256sum "$DFY_SYSTEMD_DIR/dfy-relay.service" | awk '{print $1}')" ] || fail "config failure did not restore unit"
[ "$before_binary" = "$(sha256sum "$DFY_PREFIX/dfy-relay" | awk '{print $1}')" ] || fail "config failure removed or replaced the old binary"
[ -e "$MOCK_ACTIVE" ] || fail "config failure did not restore active service"
pass "pre-install configuration failure preserves the previous binary and active service"

bash "$ROOT/deploy/install-relay.sh" --uninstall >"$TEST_ROOT/uninstall.log" 2>&1
[ ! -e "$DFY_PREFIX/dfy-relay" ] || fail "uninstall left binary"
[ ! -e "$DFY_SYSTEMD_DIR/dfy-relay.service" ] || fail "uninstall left unit"
[ -f "$DFY_CONFIG_DIR/relay.toml" ] || fail "uninstall removed config by default"
pass "safe uninstall preserves configuration"

printf 'ALL RELAY INSTALLER SMOKE TESTS PASSED\n'
