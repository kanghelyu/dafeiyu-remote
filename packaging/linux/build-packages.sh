#!/usr/bin/env bash
# SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../common.sh
source "$SCRIPT_DIR/../common.sh"

usage() {
  cat <<'EOF'
Build native Linux DEB/RPM packages from dfy and dfy-relay binaries.
  --binary-dir DIR   Directory containing both ELF binaries (required)
  --version VERSION  Semantic version, optional v prefix (required)
  --arch ARCH        x86_64 or aarch64 (required)
  --format FORMAT    deb, rpm, or all (default all)
  --output-dir DIR   Destination directory (required)
No package enables or starts the relay and no credentials are generated.
EOF
}
BINARY_DIR="" VERSION="" ARCH="" FORMAT=all OUTPUT_DIR=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --binary-dir) [ "$#" -ge 2 ] || die "--binary-dir requires a value"; BINARY_DIR="$2"; shift 2 ;;
    --version) [ "$#" -ge 2 ] || die "--version requires a value"; VERSION="$2"; shift 2 ;;
    --arch) [ "$#" -ge 2 ] || die "--arch requires a value"; ARCH="$2"; shift 2 ;;
    --format) [ "$#" -ge 2 ] || die "--format requires a value"; FORMAT="$2"; shift 2 ;;
    --output-dir) [ "$#" -ge 2 ] || die "--output-dir requires a value"; OUTPUT_DIR="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done
[ -n "$BINARY_DIR" ] && [ -n "$VERSION" ] && [ -n "$ARCH" ] && [ -n "$OUTPUT_DIR" ] || { usage >&2; exit 2; }
VERSION="$(validate_version "$VERSION")"
validate_binary_dir "$BINARY_DIR" linux
case "$ARCH" in x86_64|aarch64) ;; *) die "--arch must be x86_64 or aarch64" ;; esac
case "$FORMAT" in deb|rpm|all) ;; *) die "--format must be deb, rpm, or all" ;; esac
if [ "$FORMAT" = deb ] || [ "$FORMAT" = all ]; then need dpkg-deb; fi
if [ "$FORMAT" = rpm ] || [ "$FORMAT" = all ]; then need rpmbuild; fi
need python3
python3 "$SCRIPT_DIR/../native_binary.py" "$ARCH-unknown-linux-gnu" \
  "$BINARY_DIR/dfy" "$BINARY_DIR/dfy-relay"
ROOT="$(repo_root)"
mkdir -p "$OUTPUT_DIR"
WORK="$(mktemp -d)"
trap 'rm -rf -- "$WORK"' EXIT
STAGE="$WORK/root"
PACKAGE_PATHS=()
install -d -m 0755 "$STAGE/usr/bin" "$STAGE/usr/lib/systemd/system" "$STAGE/etc/dfy-relay" "$STAGE/usr/share/doc/dafeiyu" \
  "$STAGE/usr/share/applications" "$STAGE/usr/share/icons/hicolor/256x256/apps"
install -m 0755 "$BINARY_DIR/dfy" "$BINARY_DIR/dfy-relay" "$STAGE/usr/bin/"
install -m 0644 "$ROOT/deploy/dfy-relay.service" "$STAGE/usr/lib/systemd/system/dfy-relay.service"
install -m 0644 "$ROOT/deploy/relay.toml.example" "$STAGE/etc/dfy-relay/relay.toml.example"
install -m 0644 "$ROOT/packaging/linux/dfy-256.png" "$STAGE/usr/share/icons/hicolor/256x256/apps/dfy.png"
install -m 0644 "$ROOT/packaging/linux/dafeiyu.desktop" "$STAGE/usr/share/applications/dafeiyu.desktop"
stage_docs "$ROOT" "$STAGE/usr/share/doc/dafeiyu"

build_deb() {
  need dpkg-deb
  local deb_arch package size control
  [ "$ARCH" = x86_64 ] && deb_arch=amd64 || deb_arch=arm64
  package="$OUTPUT_DIR/dafeiyu_${VERSION}_${deb_arch}.deb"
  control="$STAGE/DEBIAN"
  install -d -m 0755 "$control"
  size="$(du -sk "$STAGE" | awk '{print $1}')"
  cat > "$control/control" <<EOF
Package: dafeiyu
Version: $VERSION
Section: net
Priority: optional
Architecture: $deb_arch
Maintainer: Dafeiyu Remote Project <noreply@users.noreply.github.com>
Depends: libc6 (>= 2.39), xvfb, x11-utils, x11-xserver-utils
Installed-Size: $size
Description: Dafeiyu Remote command-line tools and DFYP/1 relay
 Cross-platform remote-control CLI binaries and an authenticated UDP relay.
 The relay service is installed disabled; generate a relay_key before enabling it.
EOF
  install -m 0755 "$SCRIPT_DIR/debian/postinst" "$control/postinst"
  install -m 0755 "$SCRIPT_DIR/debian/prerm" "$control/prerm"
  install -m 0755 "$SCRIPT_DIR/debian/postrm" "$control/postrm"
  printf '%s\n' /etc/dfy-relay/relay.toml.example > "$control/conffiles"
  rm -f -- "$package"
  dpkg-deb --root-owner-group --build "$STAGE" "$package" >/dev/null
  [ -s "$package" ] || die "dpkg-deb did not create $package"
  PACKAGE_PATHS+=("$package")
  printf '  %s\n' "$(sha256sum "$package" | awk '{print $1}')  $(basename "$package")" > "$package.sha256"
  printf 'Created %s\n' "$package"
}

build_rpm() {
  need rpmbuild
  local rpm_arch top package
  [ "$ARCH" = x86_64 ] && rpm_arch=x86_64 || rpm_arch=aarch64
  top="$WORK/rpmbuild"
  install -d -m 0755 "$top/BUILD" "$top/BUILDROOT" "$top/RPMS" "$top/SOURCES" "$top/SPECS" "$top/SRPMS"
  cat > "$top/SPECS/dafeiyu.spec" <<EOF
%global __requires_exclude_from ^%{_bindir}/dfy(-relay)?$
Name: dafeiyu
Version: $VERSION
Release: 1
Summary: Dafeiyu Remote CLI and DFYP/1 relay
License: LicenseRef-PolyForm-Noncommercial-1.0.0
URL: https://github.com/kanghelyu/dafeiyu
BuildArch: $rpm_arch
Requires: glibc >= 2.39
Requires: coreutils
Requires: shadow-utils
Requires: glibc-common
Requires: xorg-x11-server-Xvfb
Requires: xorg-x11-utils
Requires: xorg-x11-server-utils
%description
Cross-platform remote-control command-line tools and an authenticated UDP relay.
The relay service is not enabled or started by this package.
%pre
if ! getent group dfy-relay >/dev/null 2>&1; then groupadd --system dfy-relay; fi
if ! getent passwd dfy-relay >/dev/null 2>&1; then
  useradd --system --gid dfy-relay --home-dir /nonexistent --shell /usr/sbin/nologin dfy-relay
fi
%post
if [ -d /run/systemd/system ]; then systemctl daemon-reload >/dev/null 2>&1 || true; fi
install -d -o root -g dfy-relay -m 0750 /etc/dfy-relay
%postun
if [ -d /run/systemd/system ]; then systemctl daemon-reload >/dev/null 2>&1 || true; fi
%files
%defattr(0644,root,root)
%attr(0755,root,root) /usr/bin/dfy
%attr(0755,root,root) /usr/bin/dfy-relay
/usr/lib/systemd/system/dfy-relay.service
%attr(0644,root,root) /usr/share/applications/dafeiyu.desktop
%attr(0644,root,root) /usr/share/icons/hicolor/256x256/apps/dfy.png
%config(noreplace) %attr(0644,root,root) /etc/dfy-relay/relay.toml.example
%attr(0755,root,root) %dir /usr/share/doc/dafeiyu
/usr/share/doc/dafeiyu/*
EOF
  rpmbuild --define "_topdir $top" -bb "$top/SPECS/dafeiyu.spec" >/dev/null
  package="$(find "$top/RPMS" -type f -name 'dafeiyu-*.rpm' -print -quit)"
  [ -n "$package" ] || die "rpmbuild did not create an RPM"
  cp -- "$package" "$OUTPUT_DIR/"
  package="$OUTPUT_DIR/$(basename "$package")"
  PACKAGE_PATHS+=("$package")
  printf '  %s\n' "$(sha256sum "$package" | awk '{print $1}')  $(basename "$package")" > "$package.sha256"
  printf 'Created %s\n' "$package"
}

if [ "$FORMAT" = deb ] || [ "$FORMAT" = all ]; then build_deb; fi
if [ "$FORMAT" = rpm ] || [ "$FORMAT" = all ]; then build_rpm; fi
write_build_info "$OUTPUT_DIR" "$VERSION" "$ARCH-unknown-linux-gnu" "unsigned" "BUILD-INFO-linux-${ARCH}-${FORMAT}.txt"
MANIFEST="$OUTPUT_DIR/SHA256SUMS-linux-${ARCH}-${FORMAT}.txt"
: > "$MANIFEST"
for package in "${PACKAGE_PATHS[@]}"; do
  (cd "$OUTPUT_DIR" && sha256sum "$(basename "$package")") >> "$MANIFEST"
done
