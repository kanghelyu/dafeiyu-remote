#!/usr/bin/env bash
# SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
set -euo pipefail

die() { printf 'packaging: ERROR: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "missing required tool: $1"; }

repo_root() {
  local script_dir
  script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
  cd "$script_dir/.." && pwd
}

validate_version() {
  local version="${1#v}"
  [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9.-]+)?(\+[A-Za-z0-9.-]+)?$ ]] \
    || die "version must be semantic, for example 0.1.0 or v0.1.0"
  printf '%s\n' "$version"
}

validate_arch() {
  case "$1" in x86_64|arm64|aarch64) printf '%s\n' "$1" ;; *) die "architecture must be x86_64, arm64, or aarch64" ;; esac
}

validate_binary_dir() {
  local dir="$1" os="$2"
  [ -d "$dir" ] || die "binary directory does not exist: $dir"
  if [ "$os" = windows ]; then
    [ -f "$dir/dfy.exe" ] && [ -f "$dir/dfy-relay.exe" ] || die "binary directory must contain dfy.exe and dfy-relay.exe"
  else
    [ -f "$dir/dfy" ] && [ -f "$dir/dfy-relay" ] || die "binary directory must contain dfy and dfy-relay"
  fi
}

stage_docs() {
  local root="$1" destination="$2"
  install -d -m 0755 "$destination"
  install -m 0644 "$root/README.md" "$root/LICENSE" "$destination/"
  install -m 0644 "$root/spec/DFYP-v1.md" "$destination/"
  install -m 0644 "$root/deploy/README.md" "$destination/relay-deployment.md"
}

write_build_info() {
  local output="$1" version="$2" target="$3" signing="$4" filename="${5:-BUILD-INFO.txt}"
  cat > "$output/$filename" <<EOF
Dafeiyu Remote packaging metadata
Version: $version
Target: $target
Signing: $signing
Built by: packaging scripts in the source repository
EOF
}
