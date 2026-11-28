#!/usr/bin/env bash
# SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../common.sh
source "$SCRIPT_DIR/../common.sh"

usage() {
  cat <<'EOF'
Build macOS installer packages (CLI /usr/local/bin + optional Dafeiyu Remote.app).
  --binary-dir DIR   Directory containing native dfy, dfy-relay, and optional dfy-manager (required)
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
HAS_APP=0
CHECK_BINS=("$BINARY_DIR/dfy" "$BINARY_DIR/dfy-relay")
if [ -f "$BINARY_DIR/dfy-manager" ]; then
  HAS_APP=1
  CHECK_BINS+=("$BINARY_DIR/dfy-manager")
fi
python3 "$SCRIPT_DIR/../native_binary.py" "$TARGET_ARCH-apple-darwin" "${CHECK_BINS[@]}"
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
[ -z "${MAC_APP_SIGNING_IDENTITY:-}" ] || [ -n "$IDENTITY" ] \
  || die "MAC_APP_SIGNING_IDENTITY requires MAC_SIGNING_IDENTITY"
for hook in MAC_PRE_NOTARIZE_HOOK MAC_POST_NOTARIZE_HOOK; do
  if [ -n "${!hook:-}" ]; then
    [ "$NOTARY_MODE" != none ] || die "$hook requires notarization credentials"
    [ -x "${!hook}" ] || die "$hook is not executable: ${!hook}"
  fi
done
if [ "$NOTARY_MODE" != none ]; then need xcrun; xcrun --find notarytool >/dev/null; fi

APP_BUNDLE=""
APP_COMPONENT=""
if [ "$HAS_APP" = 1 ]; then
  APP_ROOT="$WORK/app-root"
  APP_BUNDLE="$APP_ROOT/Dafeiyu Remote.app"
  install -d -m 0755 "$APP_BUNDLE/Contents/MacOS" "$APP_BUNDLE/Contents/Resources/docs"
  install -m 0755 "$BINARY_DIR/dfy-manager" "$BINARY_DIR/dfy" "$BINARY_DIR/dfy-relay" \
    "$APP_BUNDLE/Contents/MacOS/"
  install -m 0644 "$ROOT/packaging/macos/DFY.icns" "$APP_BUNDLE/Contents/Resources/DFY.icns"
  stage_docs "$ROOT" "$APP_BUNDLE/Contents/Resources/docs"
  cat > "$APP_BUNDLE/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key>
  <string>zh_CN</string>
  <key>CFBundleDisplayName</key>
  <string>大肥鱼远程</string>
  <key>CFBundleExecutable</key>
  <string>dfy-manager</string>
  <key>CFBundleIconFile</key>
  <string>DFY.icns</string>
  <key>CFBundleIdentifier</key>
  <string>remote.dafeiyu.app</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>CFBundleName</key>
  <string>大肥鱼远程</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>${VERSION}</string>
  <key>CFBundleVersion</key>
  <string>${VERSION}</string>
  <key>LSMinimumSystemVersion</key>
  <string>12.3</string>
  <key>LSUIElement</key>
  <false/>
  <key>NSHumanReadableCopyright</key>
  <string>Copyright 2026 Dafeiyu Remote contributors</string>
  <key>NSHighResolutionCapable</key>
  <true/>
  <key>NSScreenCaptureUsageDescription</key>
  <string>大肥鱼远程需要屏幕录制权限，以便作为被控端采集并传输当前显示器画面。</string>
  <key>NSMicrophoneUsageDescription</key>
  <string>大肥鱼远程需要麦克风权限，以便在启用音频传输时采集并发送本机声音。</string>
</dict>
</plist>
EOF
  chmod 0644 "$APP_BUNDLE/Contents/Info.plist"
  if [ -n "${MAC_APP_SIGNING_IDENTITY:-}" ]; then
    need codesign
    codesign --force --deep --options runtime --sign "$MAC_APP_SIGNING_IDENTITY" "$APP_BUNDLE"
  fi
fi

LABEL=unsigned
[ -z "$IDENTITY" ] || LABEL=signed
PKG="$OUTPUT_DIR/dafeiyu-${VERSION}-macos-${ARCH}-${LABEL}.pkg"
COMPONENT="$WORK/component.pkg"
pkg_args=(--root "$STAGE" --identifier remote.dafeiyu.cli --version "$VERSION" --install-location /usr/local)
[ -z "$IDENTITY" ] || pkg_args+=(--sign "$IDENTITY")
pkgbuild "${pkg_args[@]}" "$COMPONENT"
product_args=(--package "$COMPONENT")
if [ "$HAS_APP" = 1 ]; then
  APP_COMPONENT="$WORK/app-component.pkg"
  app_pkg_args=(--root "$APP_ROOT" --component-plist "$ROOT/packaging/macos/component.plist" --identifier remote.dafeiyu.app --version "$VERSION" --install-location /Applications)
  [ -z "$IDENTITY" ] || app_pkg_args+=(--sign "$IDENTITY")
  pkgbuild "${app_pkg_args[@]}" "$APP_COMPONENT"
  product_args+=(--package "$APP_COMPONENT")
fi
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
  if [ "$HAS_APP" = 1 ]; then
    cp -R -- "$APP_BUNDLE" "$DMG_SOURCE/"
    ln -s /Applications "$DMG_SOURCE/Applications"
  fi
  DMG="$OUTPUT_DIR/dafeiyu-${VERSION}-macos-${ARCH}-${LABEL}.dmg"
  rm -f -- "$DMG"
  hdiutil create -volname "大肥鱼远程 $VERSION" -srcfolder "$DMG_SOURCE" -format UDZO -ov "$DMG" >/dev/null
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
