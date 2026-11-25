#!/usr/bin/env bash
# 大肥鱼远程 · 中继服务器一键部署脚本（Linux）
#
# 用法（在 VPS 上以 root 运行）：
#   curl -fsSL https://raw.githubusercontent.com/<owner>/dafeiyu/main/deploy/install-relay.sh | sudo bash
# 或本地仓库内：
#   sudo bash deploy/install-relay.sh --from-source
#
# 脚本会：
#   1. 检查系统与依赖（curl、tar、systemd）
#   2. 安装 dfy-relay 二进制（源码构建或预编译包）
#   3. 生成 /etc/dfy-relay/relay.toml（随机 relay_key，UDP 21117 / TCP 21118 / 管理 127.0.0.1:21119）
#   4. 注册 systemd 服务 dfy-relay.service 并启动
#   5. 打印客户端连接参数，并做一次健康检查
#
# 设计原则：幂等（重复运行只会升级/更新配置）、失败即回滚、不碰系统已有软件包版本。

set -euo pipefail

REPO_OWNER="${DFY_REPO_OWNER:-kanghelyu}"
REPO_NAME="${DFY_REPO_NAME:-dafeiyu}"
VERSION="${DFY_VERSION:-latest}"
RELAY_PORT="${DFY_RELAY_PORT:-21117}"
RELAY_TCP_PORT="${DFY_RELAY_TCP_PORT:-21118}"
ADMIN_ADDR="${DFY_ADMIN_ADDR:-127.0.0.1:21119}"
PREFIX="${DFY_PREFIX:-/usr/local/bin}"
CONFIG_DIR="${DFY_CONFIG_DIR:-/etc/dfy-relay}"
SERVICE_NAME="dfy-relay.service"
FROM_SOURCE=0
YES=0

log()  { printf '\033[1;32m[dfy]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[dfy]\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m[dfy]\033[0m %s\n' "$*" >&2; exit 1; }

usage() {
  cat <<'EOF'
大肥鱼远程 · 中继一键部署

  --from-source     从源码构建（需要 git + Rust 工具链；会自动安装 rustup）
  --version <v>     指定发布版本（默认 latest，仅预编译包路径使用）
  --relay-port <p>  中继 UDP 端口（默认 21117）
  --tcp-port <p>    中继 TCP 降级端口（默认 21118）
  --yes             非交互（自动确认）
  --uninstall       卸载（保留配置目录）
  -h, --help        显示帮助

环境变量同名的 DFY_* 可覆盖默认值。
EOF
}

for arg in "$@"; do
  case "$arg" in
    --from-source) FROM_SOURCE=1 ;;
    --yes|-y) YES=1 ;;
    --uninstall) UNINSTALL=1 ;;
    -h|--help) usage; exit 0 ;;
    *) ;;
  esac
done
while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="${2:?}"; shift 2 ;;
    --relay-port) RELAY_PORT="${2:?}"; shift 2 ;;
    --tcp-port) RELAY_TCP_PORT="${2:?}"; shift 2 ;;
    *) shift ;;
  esac
done

need_root() { [ "$(id -u)" = "0" ] || die "请用 root 运行（sudo bash $0）"; }

detect_arch() {
  case "$(uname -m)" in
    x86_64|amd64) echo "x86_64" ;;
    aarch64|arm64) echo "aarch64" ;;
    *) die "不支持的架构：$(uname -m)" ;;
  esac
}

check_os() {
  [ -f /etc/os-release ] || die "无法识别系统（缺少 /etc/os-release）"
  # shellcheck disable=SC1091
  . /etc/os-release
  log "系统：${PRETTY_NAME:-$ID}"
  case "${ID:-}" in
    ubuntu|debian|centos|rhel|rocky|almalinux|fedora|alpine) ;;
    *) warn "未验证的发行版：${ID:-unknown}，继续尝试安装" ;;
  esac
}

install_deps() {
  if command -v curl >/dev/null && command -v tar >/dev/null; then return; fi
  log "安装基础依赖（curl、tar）"
  if command -v apt-get >/dev/null; then
    DEBIAN_FRONTEND=noninteractive apt-get update -y && apt-get install -y curl tar ca-certificates
  elif command -v dnf >/dev/null; then
    dnf install -y curl tar ca-certificates
  elif command -v yum >/dev/null; then
    yum install -y curl tar ca-certificates
  elif command -v apk >/dev/null; then
    apk add --no-cache curl tar ca-certificates
  else
    die "找不到包管理器，请手动安装 curl 与 tar"
  fi
}

install_from_source() {
  log "从源码构建 dfy-relay（首次可能需要几分钟）"
  if ! command -v cargo >/dev/null; then
    log "未检测到 Rust，安装 rustup（非交互）"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
  fi
  if ! command -v git >/dev/null; then
    if command -v apt-get >/dev/null; then DEBIAN_FRONTEND=noninteractive apt-get install -y git
    elif command -v dnf >/dev/null; then dnf install -y git
    else die "需要 git 拉取源码"; fi
  fi
  local tmp; tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  git clone --depth 1 "https://github.com/${REPO_OWNER}/${REPO_NAME}.git" "$tmp/src"
  ( cd "$tmp/src" && cargo build --release -p dfy-relay )
  install -m 0755 "$tmp/src/target/release/dfy-relay" "${PREFIX}/dfy-relay"
}

install_from_release() {
  local arch url tmp
  arch="$(detect_arch)"
  if [ "$VERSION" = "latest" ]; then
    url="https://github.com/${REPO_OWNER}/${REPO_NAME}/releases/latest/download/dfy-relay-linux-${arch}.tar.gz"
  else
    url="https://github.com/${REPO_OWNER}/${REPO_NAME}/releases/download/${VERSION}/dfy-relay-linux-${arch}.tar.gz"
  fi
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  log "下载预编译包：$url"
  if ! curl -fL --retry 3 -o "$tmp/relay.tar.gz" "$url"; then
    warn "预编译包不可用，回退到源码构建"
    install_from_source
    return
  fi
  tar -xzf "$tmp/relay.tar.gz" -C "$tmp"
  install -m 0755 "$tmp/dfy-relay" "${PREFIX}/dfy-relay"
}

write_config() {
  mkdir -p "$CONFIG_DIR"
  if [ -f "$CONFIG_DIR/relay.toml" ]; then
    log "保留已有配置 $CONFIG_DIR/relay.toml"
  else
    log "生成配置 $CONFIG_DIR/relay.toml"
    "${PREFIX}/dfy-relay" config init --out "$CONFIG_DIR/relay.toml" --force >/dev/null
  fi
  # 端口可通过参数覆盖（用 sed 只改 bind 行，保持其余字段不变）。
  sed -i -E "s#^bind = .*#bind = \"0.0.0.0:${RELAY_PORT}\"#" "$CONFIG_DIR/relay.toml"
  sed -i -E "s#^tcp_bind = .*#tcp_bind = \"0.0.0.0:${RELAY_TCP_PORT}\"#" "$CONFIG_DIR/relay.toml"
  sed -i -E "s#^admin = .*#admin = \"${ADMIN_ADDR}\"#" "$CONFIG_DIR/relay.toml"
  chmod 600 "$CONFIG_DIR/relay.toml"
}

write_service() {
  cat > "/etc/systemd/system/${SERVICE_NAME}" <<EOF
[Unit]
Description=DaFeiYu Remote relay (zero-knowledge UDP rendezvous)
Documentation=https://github.com/${REPO_OWNER}/${REPO_NAME}
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=${PREFIX}/dfy-relay serve --config ${CONFIG_DIR}/relay.toml
Restart=always
RestartSec=3
LimitNOFILE=65536
# 安全加固：relay 只需要网络与自己的配置
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true
ReadOnlyPaths=${CONFIG_DIR}
AmbientCapabilities=
CapabilityBoundingSet=
SystemCallFilter=@system-service
SystemCallErrorNumber=EPERM

[Install]
WantedBy=multi-user.target
EOF
  systemctl daemon-reload
  systemctl enable "$SERVICE_NAME" >/dev/null
  systemctl restart "$SERVICE_NAME"
}

firewall_hint() {
  log "提醒：请确认防火墙放行 UDP/${RELAY_PORT} 与 TCP/${RELAY_TCP_PORT}"
  if command -v ufw >/dev/null && ufw status 2>/dev/null | grep -q "Status: active"; then
    ufw allow "${RELAY_PORT}/udp" >/dev/null || true
    ufw allow "${RELAY_TCP_PORT}/tcp" >/dev/null || true
    log "已通过 ufw 放行端口"
  elif command -v firewall-cmd >/dev/null && firewall-cmd --state >/dev/null 2>&1; then
    firewall-cmd --permanent --add-port="${RELAY_PORT}/udp" >/dev/null
    firewall-cmd --permanent --add-port="${RELAY_TCP_PORT}/tcp" >/dev/null
    firewall-cmd --reload >/dev/null
    log "已通过 firewalld 放行端口"
  fi
}

health_check() {
  sleep 1
  log "健康检查：${ADMIN_ADDR}/healthz"
  if curl -fsS "http://${ADMIN_ADDR}/healthz" >/dev/null 2>&1; then
    log "中继运行正常"
  else
    warn "管理端口未响应，请查看：journalctl -u ${SERVICE_NAME} -n 50 --no-pager"
  fi
  local pub
  pub="$(curl -fsS --max-time 3 https://api.ipify.org 2>/dev/null || true)"
  [ -n "$pub" ] || pub="<你的公网IP>"
  local key
  key="$(grep -E '^relay_key' "$CONFIG_DIR/relay.toml" | sed -E 's/^relay_key = "(.*)"/\1/')"
  cat <<EOF

==================== 部署完成 ====================
中继地址（客户端填写）：${pub}:${RELAY_PORT}
relay_key（客户端填写，注意保密）：
  ${key}

客户端命令示例：
  dfy agent --relay ${pub}:${RELAY_PORT} --relay-key '${key}'
  dfy view  --relay ${pub}:${RELAY_PORT} --relay-key '${key}' --clipboard

配置文件：${CONFIG_DIR}/relay.toml（权限 600）
服务管理：systemctl status|restart|stop ${SERVICE_NAME}
管理面板：curl http://${ADMIN_ADDR}/status （仅本机）
==================================================
EOF
}

do_uninstall() {
  log "卸载 dfy-relay"
  systemctl stop "$SERVICE_NAME" 2>/dev/null || true
  systemctl disable "$SERVICE_NAME" 2>/dev/null || true
  rm -f "/etc/systemd/system/${SERVICE_NAME}"
  systemctl daemon-reload
  rm -f "${PREFIX}/dfy-relay"
  log "已卸载（配置保留在 $CONFIG_DIR，如需彻底删除：rm -rf $CONFIG_DIR）"
}

main() {
  need_root
  if [ "${UNINSTALL:-0}" = "1" ]; then do_uninstall; exit 0; fi
  check_os
  install_deps
  if [ "$FROM_SOURCE" = "1" ]; then install_from_source; else install_from_release; fi
  write_config
  write_service
  firewall_hint
  health_check
}

main "$@"