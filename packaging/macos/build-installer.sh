#!/usr/bin/env bash
# SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../common.sh
source "$SCRIPT_DIR/../common.sh"

usage() {
  cat <<'EOF'
Build macOS CLI installer packages (not a GUI application).
  --binary-dir DIR   Directory containing native dfy and dfy-relay (required)
  --version VERSION  Semantic version, optional v prefix (required)
  --arch ARCH        x86_64 or arm64 (required)
  --format FORMAT    pkg, dmg, or all (default pkg)
  --output-dir DIR   Destination directory (required)
Optional signing/notarization environment variables are documented in deploy/README.md.
EOF
}
BINARY_DIR="" VERSION="" ARCH="" FORMAT=pkg OUTPUT_DIR=""
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
ARCH="$(validate_arch "$ARCH")"
validate_binary_dir "$BINARY_DIR" macos
need pkgbuild
need productbuild
need shasum
need python3
[ "$ARCH" = aarch64 ] && ARCH=arm64
TARGET_ARCH="$ARCH"
[ "$TARGET_ARCH" = arm64 ] && TARGET_ARCH=aarch64
python3 "$SCRIPT_DIR/../native_binary.py" "$TARGET_ARCH-apple-darwin" \
  "$BINARY_DIR/dfy" "$BINARY_DIR/dfy-relay"
if [ "$FORMAT" = dmg ] || [ "$FORMAT" = all ]; then need hdiutil; fi
ROOT="$(repo_root)"
mkdir -p "$OUTPUT_DIR"
WORK="$(mktemp -d)"
trap 'rm -rf -- "$WORK"' EXIT
STAGE="$WORK/root"
install -d -m 0755 "$STAGE/bin" "$STAGE/share/doc/dafeiyu"
install -m 0755 "$BINARY_DIR/dfy" "$BINARY_DIR/dfy-relay" "$STAGE/bin/"
install -m 0644 "$ROOT/packaging/macos/DFY.icns" "$STAGE/share/doc/dafeiyu/DFY.icns"
stage_docs "$ROOT" "$STAGE/share/doc/dafeiyu"

IDENTITY="${MAC_SIGNING_IDENTITY:-}"
NOTARY_MODE=none
if [ -n "${MAC_NOTARY_KEY_BASE64:-}" ]; then
  need openssl
  [[ "${MAC_NOTARY_KEY_ID:-}" =~ ^[A-Za-z0-9]+$ ]] || die "MAC_NOTARY_KEY_ID must be alphanumeric"
  MAC_NOTARY_KEY_PATH="$WORK/AuthKey_${MAC_NOTARY_KEY_ID}.p8"
  printf '%s' "$MAC_NOTARY_KEY_BASE64" | openssl base64 -d -A > "$MAC_NOTARY_KEY_PATH"
  chmod 0600 "$MAC_NOTARY_KEY_PATH"
fi
if [ -n "${MAC_NOTARY_KEY_PATH:-}" ] || [ -n "${MAC_NOTARY_KEY_ID:-}" ] || [ -n "${MAC_NOTARY_ISSUER_ID:-}" ]; then
  [ -n "$IDENTITY" ] || die "notarization credentials require MAC_SIGNING_IDENTITY"
  [ -n "${MAC_NOTARY_KEY_PATH:-}" ] && [ -n "${MAC_NOTARY_KEY_ID:-}" ] && [ -n "${MAC_NOTARY_ISSUER_ID:-}" ] \
    || die "notary API-key credentials are incomplete"
  [ -f "$MAC_NOTARY_KEY_PATH" ] || die "MAC_NOTARY_KEY_PATH does not exist"
  NOTARY_MODE=api-key
elif [ -n "${APPLE_ID:-}" ] || [ -n "${APPLE_PASSWORD:-}" ] || [ -n "${APPLE_TEAM_ID:-}" ]; then
  [ -n "$IDENTITY" ] || die "notarization credentials require MAC_SIGNING_IDENTITY"
  [ -n "${APPLE_ID:-}" ] && [ -n "${APPLE_PASSWORD:-}" ] && [ -n "${APPLE_TEAM_ID:-}" ] \
    || die "Apple ID notarization credentials are incomplete"
  NOTARY_MODE=apple-id
fi
[[ "${MAC_REQUIRE_NOTARIZATION:-0}" =~ ^[01]$ ]] || die "MAC_REQUIRE_NOTARIZATION must be 0 or 1"
if [ "${MAC_REQUIRE_NOTARIZATION:-0}" = 1 ] && [ "$NOTARY_MODE" = none ]; then
  die "MAC_REQUIRE_NOTARIZATION=1 but no complete notarization credentials were supplied"
fi
[ -z "${MAC_DMG_SIGNING_IDENTITY:-}" ] || [ -n "$IDENTITY" ] \
  || die "MAC_DMG_SIGNING_IDENTITY requires MAC_SIGNING_IDENTITY"
for hook in MAC_PRE_NOTARIZE_HOOK MAC_POST_NOTARIZE_HOOK; do
  if [ -n "${!hook:-}" ]; then
    [ "$NOTARY_MODE" != none ] || die "$hook requires notarization credentials"
    [ -x "${!hook}" ] || die "$hook is not executable: ${!hook}"
  fi
done
if [ "$NOTARY_MODE" != none ]; then need xcrun; xcrun --find notarytool >/dev/null; fi

LABEL=unsigned
[ -z "$IDENTITY" ] || LABEL=signed
PKG="$OUTPUT_DIR/dafeiyu-${VERSION}-macos-${ARCH}-${LABEL}.pkg"
COMPONENT="$WORK/component.pkg"
pkg_args=(--root "$STAGE" --identifier remote.dafeiyu.cli --version "$VERSION" --install-location /usr/local)
[ -z "$IDENTITY" ] || pkg_args+=(--sign "$IDENTITY")
pkgbuild "${pkg_args[@]}" "$COMPONENT"
product_args=(--package "$COMPONENT")
[ -z "$IDENTITY" ] || product_args+=(--sign "$IDENTITY")
productbuild "${product_args[@]}" "$PKG"
[ -s "$PKG" ] || die "productbuild did not create $PKG"

notarize() {
  local artifact="$1"
  [ "$IDENTITY" != "" ] || die "internal error: cannot notarize unsigned artifact"
  [ "$NOTARY_MODE" != none ] || return 0
  [ -z "${MAC_PRE_NOTARIZE_HOOK:-}" ] || "$MAC_PRE_NOTARIZE_HOOK" "$artifact"
  local args=(submit "$artifact" --wait)
  if [ "$NOTARY_MODE" = api-key ]; then
    args+=(--key "$MAC_NOTARY_KEY_PATH" --key-id "$MAC_NOTARY_KEY_ID" --issuer "$MAC_NOTARY_ISSUER_ID")
  else
    args+=(--apple-id "$APPLE_ID" --password "$APPLE_PASSWORD" --team-id "$APPLE_TEAM_ID")
  fi
  xcrun notarytool "${args[@]}"
  xcrun stapler staple "$artifact"
  [ -n "${MAC_POST_NOTARIZE_HOOK:-}" ] && "$MAC_POST_NOTARIZE_HOOK" "$artifact"
  NOTARIZED=yes
}
NOTARIZED=no
if [ "$NOTARY_MODE" != none ]; then notarize "$PKG"; fi

if [ "$FORMAT" = dmg ] || [ "$FORMAT" = all ]; then
  DMG_SOURCE="$WORK/dmg-source"
  mkdir -p "$DMG_SOURCE"
  cp -- "$PKG" "$DMG_SOURCE/"
  DMG="$OUTPUT_DIR/dafeiyu-${VERSION}-macos-${ARCH}-${LABEL}.dmg"
  rm -f -- "$DMG"
  hdiutil create -volname "Dafeiyu Remote $VERSION" -srcfolder "$DMG_SOURCE" -format UDZO -ov "$DMG" >/dev/null
  if [ -n "${MAC_DMG_SIGNING_IDENTITY:-}" ]; then
    need codesign
    codesign --force --sign "$MAC_DMG_SIGNING_IDENTITY" "$DMG"
  fi
  [ -s "$DMG" ] || die "hdiutil did not create $DMG"
  # A separately created DMG is not covered by stapling the enclosed PKG.
  if [ "$NOTARY_MODE" != none ]; then notarize "$DMG"; fi
  printf '%s  %s\n' "$(shasum -a 256 "$DMG" | awk '{print $1}')" "$(basename "$DMG")" > "$DMG.sha256"
  printf 'Created %s\n' "$DMG"
fi
printf '%s  %s\n' "$(shasum -a 256 "$PKG" | awk '{print $1}')" "$(basename "$PKG")" > "$PKG.sha256"
printf 'Created %s\n' "$PKG"
SIGNING="$LABEL"
if [ "$NOTARY_MODE" != none ]; then
  SIGNING="signed and notarized/stapled"
elif [ "$LABEL" = signed ]; then
  SIGNING="signed (not notarized)"
fi
write_build_info "$OUTPUT_DIR" "$VERSION" "$TARGET_ARCH-apple-darwin" "$SIGNING" "BUILD-INFO-macos-${ARCH}.txt"
