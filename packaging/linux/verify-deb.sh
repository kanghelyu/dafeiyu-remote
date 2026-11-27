#!/usr/bin/env bash
# SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../common.sh
source "$SCRIPT_DIR/../common.sh"

usage() {
  cat <<'EOF'
Verify a built dafeiyu .deb without installing it.
  --package FILE   .deb to inspect (required)
  --version VER    expected Version (required)
  --arch ARCH      expected Architecture: amd64|arm64 (required)
EOF
}
PACKAGE="" VERSION="" ARCH=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --package) [ "$#" -ge 2 ] || die "--package requires a value"; PACKAGE="$2"; shift 2 ;;
    --version) [ "$#" -ge 2 ] || die "--version requires a value"; VERSION="$2"; shift 2 ;;
    --arch) [ "$#" -ge 2 ] || die "--arch requires a value"; ARCH="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done
[ -n "$PACKAGE" ] && [ -n "$VERSION" ] && [ -n "$ARCH" ] || { usage >&2; exit 2; }
need dpkg-deb
[ -s "$PACKAGE" ] || die "package not found: $PACKAGE"

info="$(dpkg-deb -I "$PACKAGE")"
files="$(dpkg-deb -c "$PACKAGE")"
check_info() { printf '%s\n' "$info" | grep -Eq "$1" || die "missing control field: $2"; }
check_file() { printf '%s\n' "$files" | grep -Eq "$1" || die "missing package file: $2"; }

check_info "^[[:space:]]*Package: dafeiyu$" "Package"
check_info "^[[:space:]]*Version: ${VERSION}$" "Version"
check_info "^[[:space:]]*Architecture: ${ARCH}$" "Architecture"
check_info "^[[:space:]]*Depends: .*xvfb" "xvfb dependency"
check_info "^[[:space:]]*Depends: .*x11-utils" "x11-utils dependency"
check_file "/usr/bin/dfy$" "dfy"
check_file "/usr/bin/dfy-relay$" "dfy-relay"
check_file "/usr/lib/systemd/system/dfy-relay.service$" "systemd unit"
check_file "/etc/dfy-relay/relay.toml.example$" "relay config example"
check_file "/usr/share/doc/dafeiyu/LICENSE$" "license text"
check_file "/usr/share/doc/dafeiyu/README.md$" "README"
check_file "/usr/share/applications/dafeiyu.desktop$" "desktop entry"
check_file "/usr/share/icons/hicolor/256x256/apps/dfy.png$" "application icon"

# Maintainer scripts live in the control archive, not the data archive.
CTRLDIR="$(mktemp -d)"
trap 'rm -rf -- "$CTRLDIR"' EXIT
dpkg-deb -e "$PACKAGE" "$CTRLDIR" >/dev/null
for script in postinst prerm postrm; do
  [ -f "$CTRLDIR/$script" ] || die "missing maintainer script: $script"
done

sumfile="${PACKAGE}.sha256"
if [ -s "$sumfile" ]; then
  expected="$(awk '{print $1}' "$sumfile")"
  actual="$(sha256sum "$PACKAGE" | awk '{print $1}')"
  [ "$expected" = "$actual" ] || die "sha256 mismatch"
fi

printf 'DEB verification PASS: %s (version=%s arch=%s)\n' "$(basename "$PACKAGE")" "$VERSION" "$ARCH"
