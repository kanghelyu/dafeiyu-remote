#!/usr/bin/env bash
# SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../common.sh
source "$SCRIPT_DIR/../common.sh"

usage() {
  cat <<'EOF'
Build an Inno Setup installer for dfy.exe and dfy-relay.exe.
  --binary-dir DIR   Directory containing both Windows binaries (required)
  --version VERSION  Semantic version, optional v prefix (required)
  --output-dir DIR   Destination directory (required)
EOF
}
BINARY_DIR="" VERSION="" OUTPUT_DIR=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --binary-dir) [ "$#" -ge 2 ] || die "--binary-dir requires a value"; BINARY_DIR="$2"; shift 2 ;;
    --version) [ "$#" -ge 2 ] || die "--version requires a value"; VERSION="$2"; shift 2 ;;
    --output-dir) [ "$#" -ge 2 ] || die "--output-dir requires a value"; OUTPUT_DIR="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done
[ -n "$BINARY_DIR" ] && [ -n "$VERSION" ] && [ -n "$OUTPUT_DIR" ] || { usage >&2; exit 2; }
VERSION="$(validate_version "$VERSION")"
validate_binary_dir "$BINARY_DIR" windows
ROOT="$(repo_root)"
ISCC_BIN="${ISCC:-}"
if [ -z "$ISCC_BIN" ]; then
  if command -v iscc >/dev/null 2>&1; then ISCC_BIN="$(command -v iscc)"
  elif command -v ISCC.exe >/dev/null 2>&1; then ISCC_BIN="$(command -v ISCC.exe)"
  elif [ -x "/c/Program Files (x86)/Inno Setup 6/ISCC.exe" ]; then ISCC_BIN="/c/Program Files (x86)/Inno Setup 6/ISCC.exe"
  elif [ -x "/c/Program Files/Inno Setup 6/ISCC.exe" ]; then ISCC_BIN="/c/Program Files/Inno Setup 6/ISCC.exe"
  elif [ -n "${LOCALAPPDATA:-}" ] && command -v cygpath >/dev/null 2>&1 && [ -x "$(cygpath -u "$LOCALAPPDATA/Programs/Inno Setup 6/ISCC.exe")" ]; then
    ISCC_BIN="$(cygpath -u "$LOCALAPPDATA/Programs/Inno Setup 6/ISCC.exe")"
  else die "missing required tool: Inno Setup 6 ISCC.exe (set ISCC to its full path)"; fi
fi
[ -f "$ISCC_BIN" ] || die "Inno Setup compiler not found: $ISCC_BIN"
need cygpath
need python3
python3 "$SCRIPT_DIR/../native_binary.py" x86_64-pc-windows-msvc \
  "$BINARY_DIR/dfy.exe" "$BINARY_DIR/dfy-relay.exe"

STAGE="$(mktemp -d)"
trap 'rm -rf -- "$STAGE"' EXIT
install -m 0755 "$BINARY_DIR/dfy.exe" "$BINARY_DIR/dfy-relay.exe" "$STAGE/"
ISCC_EXTRA_ARGS=()
if [ -f "$BINARY_DIR/dfy-manager.exe" ]; then
  python3 "$SCRIPT_DIR/../native_binary.py" x86_64-pc-windows-msvc "$BINARY_DIR/dfy-manager.exe"
  install -m 0755 "$BINARY_DIR/dfy-manager.exe" "$STAGE/"
  ISCC_EXTRA_ARGS+=("/DHasManager=1")
fi
install -m 0644 "$ROOT/packaging/windows/dfy.ico" "$STAGE/dfy.ico"
stage_docs "$ROOT" "$STAGE/docs"
mkdir -p "$OUTPUT_DIR"
SOURCE_WIN="$(cygpath -aw "$STAGE")"
OUTPUT_WIN="$(cygpath -aw "$OUTPUT_DIR")"
# Inno's /D switches must not be rewritten as Git Bash POSIX paths.
MSYS2_ARG_CONV_EXCL='*' "$ISCC_BIN" \
  "/DSourceDir=$SOURCE_WIN" "/DOutputDir=$OUTPUT_WIN" "/DAppVersion=$VERSION" \
  ${ISCC_EXTRA_ARGS[@]+"${ISCC_EXTRA_ARGS[@]}"} \
  "$(cygpath -aw "$SCRIPT_DIR/dfy.iss")"
INSTALLER="$OUTPUT_DIR/dafeiyu-${VERSION}-windows-x86_64-setup.exe"
[ -s "$INSTALLER" ] || die "Inno Setup returned success without producing $INSTALLER"
printf '%s  %s\n' "$(sha256sum "$INSTALLER" | awk '{print $1}')" "$(basename "$INSTALLER")" \
  > "$INSTALLER.sha256"
write_build_info "$OUTPUT_DIR" "$VERSION" x86_64-pc-windows-msvc "unsigned (no Windows signing secret supported)" "BUILD-INFO-windows-x86_64.txt"
printf 'Created %s\n' "$INSTALLER"
