#!/usr/bin/env bash
# SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../common.sh
source "$SCRIPT_DIR/../common.sh"

usage() {
  cat <<'EOF'
Build Android debug APK and release APK/AAB without modifying android/ sources.
  --android-dir DIR   Android Gradle project (default: repository/android)
  --output-dir DIR   Destination directory (required)
  --signing MODE     auto (default), required, or none
  --version VERSION  Artifact label (default: read from app/build.gradle.kts)

Auto signs release only when all four secrets are present; otherwise release
artifacts are explicitly named unsigned. required fails on incomplete credentials.
Secrets: ANDROID_KEYSTORE_BASE64 (or ANDROID_KEYSTORE_PATH),
ANDROID_KEYSTORE_PASSWORD, ANDROID_KEY_ALIAS, ANDROID_KEY_PASSWORD.
EOF
}
ANDROID_DIR="" OUTPUT_DIR="" SIGNING=auto VERSION=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --android-dir) [ "$#" -ge 2 ] || die "--android-dir requires a value"; ANDROID_DIR="$2"; shift 2 ;;
    --output-dir) [ "$#" -ge 2 ] || die "--output-dir requires a value"; OUTPUT_DIR="$2"; shift 2 ;;
    --signing) [ "$#" -ge 2 ] || die "--signing requires a value"; SIGNING="$2"; shift 2 ;;
    --version) [ "$#" -ge 2 ] || die "--version requires a value"; VERSION="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done
[ -n "$OUTPUT_DIR" ] || { usage >&2; exit 2; }
case "$SIGNING" in auto|required|none) ;; *) die "--signing must be auto, required, or none" ;; esac
ROOT="$(repo_root)"
ANDROID_DIR="${ANDROID_DIR:-$ROOT/android}"
[ -x "$ANDROID_DIR/gradlew" ] || die "Android Gradle wrapper not found: $ANDROID_DIR/gradlew"
need python3
mkdir -p "$OUTPUT_DIR"
WORK="$(mktemp -d)"
trap 'rm -rf -- "$WORK"' EXIT
KEYSTORE=""
CREDENTIAL_COUNT=0
for name in ANDROID_KEYSTORE_PASSWORD ANDROID_KEY_ALIAS ANDROID_KEY_PASSWORD; do
  [ -z "${!name:-}" ] || CREDENTIAL_COUNT=$((CREDENTIAL_COUNT + 1))
done
if [ -n "${ANDROID_KEYSTORE_BASE64:-}" ] || [ -n "${ANDROID_KEYSTORE_PATH:-}" ]; then
  CREDENTIAL_COUNT=$((CREDENTIAL_COUNT + 1))
fi
if [ "$CREDENTIAL_COUNT" -gt 0 ] && [ "$CREDENTIAL_COUNT" -lt 4 ]; then
  [ "$SIGNING" != auto ] || die "partial Android signing credentials; provide all four or none"
fi
if [ "$SIGNING" = required ] && [ "$CREDENTIAL_COUNT" -ne 4 ]; then
  die "--signing required but Android signing credentials are absent"
fi
if [ "$CREDENTIAL_COUNT" -eq 4 ] && [ "$SIGNING" != none ]; then
  if [ -n "${ANDROID_KEYSTORE_BASE64:-}" ]; then
    KEYSTORE="$WORK/release.jks"
    python3 -c 'import base64,os,pathlib,sys; pathlib.Path(sys.argv[1]).write_bytes(base64.b64decode(os.environ["ANDROID_KEYSTORE_BASE64"], validate=True))' "$KEYSTORE"
  else
    KEYSTORE="$ANDROID_KEYSTORE_PATH"
  fi
  [ -s "$KEYSTORE" ] || die "Android keystore is empty or missing"
  if [ -n "${ANDROID_KEYSTORE_BASE64:-}" ]; then chmod 0600 "$KEYSTORE"; fi
else
  SIGNING=none
  if [ "$CREDENTIAL_COUNT" -eq 0 ]; then
    printf '[android] release signing credentials absent; producing honestly labeled unsigned artifacts\n' >&2
  else
    printf '[android] release signing explicitly disabled; producing honestly labeled unsigned artifacts\n' >&2
  fi
fi

if [ -z "$VERSION" ]; then
  VERSION="$(python3 - "$ANDROID_DIR/app/build.gradle.kts" <<'PY'
from pathlib import Path
import re, sys
text = Path(sys.argv[1]).read_text(encoding="utf-8")
match = re.search(r'versionName\s*=\s*"([^"]+)"', text)
if not match:
    raise SystemExit("cannot read versionName; pass --version")
print(match.group(1))
PY
)"
fi
[[ "$VERSION" =~ ^[A-Za-z0-9][A-Za-z0-9._+-]*$ ]] || die "invalid artifact version label"

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
GRADLE_ARGS=(--no-daemon --stacktrace clean assembleDebug assembleRelease bundleRelease)
SDK_HOME="${ANDROID_SDK_ROOT:-${ANDROID_HOME:-}}"
if [ -n "$SDK_HOME" ]; then GRADLE_ARGS+=("-Pandroid.sdk.path=$SDK_HOME"); fi
if [ "$SIGNING" = required ] || [ "$SIGNING" = auto ] && [ -n "$KEYSTORE" ]; then
  export DFY_KEYSTORE_PATH="$KEYSTORE"
  export DFY_KEYSTORE_PASSWORD="${ANDROID_KEYSTORE_PASSWORD}"
  export DFY_KEY_ALIAS="${ANDROID_KEY_ALIAS}"
  export DFY_KEY_PASSWORD="${ANDROID_KEY_PASSWORD}"
  GRADLE_ARGS+=(--init-script "$SCRIPT_DIR/signing.gradle")
  RELEASE_SIGNING=signed
else
  RELEASE_SIGNING=unsigned
fi
( cd "$ANDROID_DIR" && ./gradlew "${GRADLE_ARGS[@]}" )

copy_artifact() {
  local source="$1" destination="$2"
  [ -s "$source" ] || die "Gradle returned success without producing $source"
  python3 - "$source" <<'PY'
from pathlib import Path
import sys, zipfile
path = Path(sys.argv[1])
if not zipfile.is_zipfile(path):
    raise SystemExit(f"not a valid ZIP-based Android artifact: {path}")
with zipfile.ZipFile(path) as bundle:
    bad = bundle.testzip()
    if bad:
        raise SystemExit(f"corrupt ZIP member: {bad}")
    names = set(bundle.namelist())
    prefix = "base/" if path.suffix == ".aab" else ""
    required = {prefix + "manifest/AndroidManifest.xml"} if prefix else {"AndroidManifest.xml", "resources.arsc", "classes.dex"}
    required |= {prefix + f"lib/{abi}/libdfy_android.so" for abi in ("arm64-v8a", "x86_64")}
    missing = required - names
    if missing:
        raise SystemExit(f"Android artifact {path} is missing required payload: {sorted(missing)}")
PY
  cp -- "$source" "$destination"
  printf '%s  %s\n' "$(sha256sum "$destination" | awk '{print $1}')" "$(basename "$destination")" > "$destination.sha256"
}

APK_DIR="$ANDROID_DIR/app/build/outputs/apk"
BUNDLE_DIR="$ANDROID_DIR/app/build/outputs/bundle"
DEBUG_APK="$APK_DIR/debug/app-debug.apk"
[ -s "$DEBUG_APK" ] || die "debug APK was not generated"
if [ "$RELEASE_SIGNING" = signed ]; then
  RELEASE_APK="$APK_DIR/release/app-release.apk"
else
  RELEASE_APK="$APK_DIR/release/app-release-unsigned.apk"
fi
[ -s "$RELEASE_APK" ] || die "expected release APK was not generated: $RELEASE_APK"
RELEASE_AAB="$BUNDLE_DIR/release/app-release.aab"
[ -s "$RELEASE_AAB" ] || die "release AAB was not generated"

copy_artifact "$DEBUG_APK" "$OUTPUT_DIR/dafeiyu-${VERSION}-debug.apk"
LABEL=""
[ "$RELEASE_SIGNING" != unsigned ] || LABEL="-unsigned"
copy_artifact "$RELEASE_APK" "$OUTPUT_DIR/dafeiyu-${VERSION}-release${LABEL}.apk"
copy_artifact "$RELEASE_AAB" "$OUTPUT_DIR/dafeiyu-${VERSION}-release${LABEL}.aab"

if [ "$RELEASE_SIGNING" = signed ]; then
  SDK_DIR="${ANDROID_SDK_ROOT:-${ANDROID_HOME:-}}"
  if [ -z "$SDK_DIR" ] && [ -f "$ANDROID_DIR/local.properties" ]; then
    SDK_DIR="$(python3 - "$ANDROID_DIR/local.properties" <<'PY'
from pathlib import Path
import sys
for line in Path(sys.argv[1]).read_text().splitlines():
    if line.startswith("sdk.dir="):
        print(line.split("=", 1)[1])
        break
PY
)"
  fi
  APKSIGNER="$(find "${SDK_DIR:-/nonexistent}/build-tools" -maxdepth 2 -name apksigner -type f 2>/dev/null | sort -V | tail -n 1 || true)"
  [ -n "$APKSIGNER" ] || die "signed APK produced but apksigner was not found under ANDROID_SDK_ROOT/build-tools"
  "$APKSIGNER" verify --verbose "$OUTPUT_DIR/dafeiyu-${VERSION}-release.apk"
  need jarsigner
  jarsigner -verify "$OUTPUT_DIR/dafeiyu-${VERSION}-release.aab" >/dev/null
fi
cat > "$OUTPUT_DIR/BUILD-INFO.txt" <<EOF
Dafeiyu Android packaging metadata
Version: $VERSION
Debug APK: signed with the project's debug key
Release APK/AAB: $RELEASE_SIGNING
Signing secrets: supplied through environment variables, never repository files
EOF
printf 'Created Android artifacts in %s\n' "$OUTPUT_DIR"
