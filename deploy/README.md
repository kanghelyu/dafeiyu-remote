# 部署中继服务器（大肥鱼远程）

中继（relay）是 DFYP/1 的 UDP 会合点：按 `tag` 成对转发报文，不解析 QUIC、不终结 TLS。
TCP 降级通道可用于 UDP 不可达的环境。`relay_key` 只用于会合鉴权，不参与媒体加密。

> 验证范围：本仓库提供脚本、配置和打包器；本文没有声称已在公网 VPS 上完成部署测试。
> 首次部署请先在可回收的测试主机上验证发行版、systemd 版本、防火墙和云厂商安全组。

## 安装方式一：Linux 一键脚本

适用于使用 systemd 的 Linux x86_64 / aarch64。脚本会安装依赖、下载并校验发布包、
生成配置、安装原子更新的二进制、注册加固的 systemd 单元，并进行 `/healthz` 检查。
健康检查失败时返回非零；升级失败会尝试恢复旧二进制。

```bash
# 建议先下载审阅，再执行；不要把管理端口暴露到公网。
curl -fsSL https://raw.githubusercontent.com/kanghelyu/dafeiyu-remote/main/deploy/install-relay.sh -o install-relay.sh
sudo bash install-relay.sh --version v0.1.0
```

发布资产名称必须与脚本一致：

- `dfy-relay-linux-x86_64.tar.gz`
- `dfy-relay-linux-aarch64.tar.gz`
- 每个归档的 `<归档名>.sha256`，或包含相同文件名的 release 级 `SHA256SUMS.txt`

脚本优先使用逐归档 `.sha256`，其次使用 `SHA256SUMS.txt`。两者都没有时默认警告并继续；
生产环境建议强制校验：

```bash
sudo DFY_REQUIRE_CHECKSUM=1 bash install-relay.sh --version v0.1.0
```

默认从 GitHub Releases 下载，**不会在下载失败时静默切换到源码构建**。也可显式从源码构建：

```bash
# 需要预先安装 git 和 Rust cargo；脚本不会自动安装 rustup。
sudo bash install-relay.sh --from-source
sudo DFY_VERSION=v0.1.0 DFY_CARGO_TARGET_DIR=/var/cache/dafeiyu/target \
  bash install-relay.sh --from-source
```

### 参数与 `DFY_*` 覆盖

| 参数 | 环境变量 | 默认值 / 行为 |
| --- | --- | --- |
| `--version` | `DFY_VERSION` | `latest` 或 `vX.Y.Z` |
| `--relay-port` | `DFY_RELAY_PORT` | UDP `21117` |
| `--tcp-port` | `DFY_RELAY_TCP_PORT` | TCP `21118`；空字符串禁用 |
| `--admin-addr` | `DFY_ADMIN_ADDR` | `127.0.0.1:21119`，必须回环 |
| `--max-sessions` | `DFY_MAX_SESSIONS` | `1024` |
| `--per-tag-mbps` | `DFY_PER_TAG_MBPS` | `200`；`0` 不限速 |
| `--tag-ttl-hours` | `DFY_TAG_TTL_HOURS` | `48` |
| 无 | `DFY_PREFIX` | `/usr/local/bin` |
| 无 | `DFY_CONFIG_DIR` | `/etc/dfy-relay` |
| 无 | `DFY_SYSTEMD_DIR` | `/etc/systemd/system` |
| 无 | `DFY_OS_RELEASE_FILE` | `/etc/os-release`；主要供隔离测试覆盖 |
| 无 | `DFY_REPO_OWNER` / `DFY_REPO_NAME` | `kanghelyu/dafeiyu-remote` |
| 无 | `DFY_RELEASE_BASE_URL` | HTTPS 的 `releases/download/VERSION` 目录 |
| 无 | `DFY_ARCHIVE_URL` | 完整 HTTPS 归档 URL |
| 无 | `DFY_REQUIRE_CHECKSUM` | `0`；设为 `1` 时无校验和即失败 |
| 无 | `DFY_NO_FIREWALL` | `0`；设为 `1` 不修改防火墙 |
| 无 | `DFY_HEALTH_RETRIES` / `DFY_HEALTH_INTERVAL` | `10` 次 / `1` 秒 |
| `--yes` | 无 | 兼容旧用法；安装本来就是非交互的 |

已有 `relay.toml` 默认完整保留。只有显式提供对应端口、地址或限制参数时，脚本才用
Python 3.11+ 的 TOML 解析器原子更新该字段，并保留所有无关字段。首次配置由
`dfy-relay config init` 生成随机 32 字节密钥，绝不使用仓库中的固定凭据。

示例：

```bash
sudo DFY_MAX_SESSIONS=2048 DFY_PER_TAG_MBPS=100 DFY_TAG_TTL_HOURS=24 \
  bash install-relay.sh --version v0.1.0
sudo bash install-relay.sh --admin-addr 127.0.0.1:22119 --relay-port 31117
```

配置文件最终为 `0640 root:dfy-relay`，服务以专用系统用户运行。若使用 `key_file`，
密钥文件也必须允许 `dfy-relay` 用户读取。部署后检查：

```bash
sudo dfy-relay config check --config /etc/dfy-relay/relay.toml
curl -fsS http://127.0.0.1:21119/healthz
curl -fsS http://127.0.0.1:21119/status
journalctl -u dfy-relay -n 100 --no-pager
```

防火墙只放行 UDP `21117` 和 TCP `21118`；管理端口仅绑定回环，不要映射到公网。

### 卸载

```bash
sudo bash install-relay.sh --uninstall
```

默认停止并禁用服务、删除 unit 和二进制，**保留 `/etc/dfy-relay` 配置及密钥**。
只有显式设置 `DFY_PURGE_CONFIG=1` 且配置目录仍为受支持的默认路径时才删除配置。

## 安装方式二：Linux `.deb` / `.rpm`

Release workflow 在 Ubuntu 上构建 `amd64` 和 `arm64` 的 `.deb` / `.rpm`。软件包安装：

- `/usr/bin/dfy` 与 `/usr/bin/dfy-relay`；
- `/usr/share/doc/dafeiyu/` 下的许可证、协议和部署说明；
- `/etc/dfy-relay/relay.toml.example`（无凭据示例）；
- `/usr/lib/systemd/system/dfy-relay.service`。

`.deb` / `.rpm` 声明 Linux 虚拟显示所需依赖：`xvfb`、`xdpyinfo`（DEB `x11-utils` /
RPM `xorg-x11-utils`）和 `xrandr`（DEB `x11-xserver-utils` / RPM
`xorg-x11-server-utils`）。当前 Linux 虚拟显示是 Agent 进程持有的隔离 X11 Xvfb，
可无物理显示器采集；它不是 DRM/Wayland 扩展显示器，也不会改变物理桌面拓扑。

安装软件包**不会自动启用或启动 relay，也不会生成密钥**。启用前需创建配置：

```bash
sudo dfy-relay config init --out /etc/dfy-relay/relay.toml
sudo chown root:dfy-relay /etc/dfy-relay/relay.toml
sudo chmod 0640 /etc/dfy-relay/relay.toml
sudo dfy-relay config check --config /etc/dfy-relay/relay.toml
sudo systemctl enable --now dfy-relay
```

## 安装方式三：Docker Compose

```bash
git clone https://github.com/kanghelyu/dafeiyu-remote.git
cd dafeiyu-remote
cargo run -p dfy-relay -- config init --out deploy/relay.toml
docker compose -f deploy/docker-compose.yml up -d
docker compose -f deploy/docker-compose.yml exec relay \
  wget -qO- http://127.0.0.1:21119/healthz
```

Compose 从仓库构建镜像，只映射数据面端口；管理端口映射到宿主机回环。生成的
`deploy/relay.toml` 含密钥且被 `.gitignore` 忽略，不要提交。

## Windows / macOS 安装包

- Windows Release 提供 Inno Setup 生成的 `dafeiyu-*-windows-x86_64-setup.exe`，安装
  `dfy.exe`、`dfy-relay.exe`、许可证和文档，并创建指向安装目录的快捷方式。
  **不安装 Windows 服务包装器**；relay 需在前台运行或由管理员另行配置服务。
- macOS Release 按 x86_64 / arm64 提供 `.pkg` 和含该 `.pkg` 的 `.dmg`，安装命令行
  `dfy`、`dfy-relay` 到 `/usr/local/bin`。本项目没有随包附带原生 GUI macOS app。
- 未配置签名身份时，macOS 文件名明确包含 `unsigned`；配置身份但未配置公证凭据时
  标记为已签名、未公证。Android 本地构建未提供完整签名秘密时，输出明确带 `unsigned`；
  Release workflow 使用 `--signing required`，凭据缺失即失败，不发布未签名 Android 包。

## 当前配置字段

由 `dfy-relay config init` 生成的字段如下（真实文件必须含 `relay_key` 或 `key_file`）：

```toml
bind = "0.0.0.0:21117"
tcp_bind = "0.0.0.0:21118"  # 空字符串可禁用 TCP
admin_addr = "127.0.0.1:21119"  # 必须回环，或留空禁用管理接口
relay_key = "base64:..."   # 32 字节；也可改用 key_file
max_sessions = 1024
per_tag_mbps = 200         # 0 = 不限速
tag_ttl_hours = 48
idle_timeout_s = 60
bind_window_s = 300
rebind_guard_ms = 2000
log = "info"
log_json = false
no_log_ips = true
```

旧名称 `admin`、`max_tags`、`per_tag_bps`、`tag_ttl_s` 不是当前字段。

## 本地构建与 CI 构建

所有本地构建都应把 `CARGO_TARGET_DIR` 指向工作区外的独立目录。打包器只接收已经构建好的
两个二进制，不会伪造编译成功：

```bash
CARGO_TARGET_DIR=/path/to/dfy-target cargo build --release --locked -p dfy-cli -p dfy-relay
python3 packaging/make_archive.py --binary-dir /path/to/dfy-target/release \
  --target x86_64-unknown-linux-gnu --version 0.1.0 --output-dir dist

# Linux 上生成 DEB / RPM；缺少工具时会明确失败。
packaging/linux/build-packages.sh --binary-dir /path/to/dfy-target/release \
  --version 0.1.0 --arch x86_64 --format all --output-dir dist
# Windows + Inno Setup 6：
packaging/windows/build-installer.sh --binary-dir /path/to/release \
  --version 0.1.0 --output-dir dist
# macOS + Xcode command-line tools：
packaging/macos/build-installer.sh --binary-dir /path/to/release \
  --version 0.1.0 --arch arm64 --format all --output-dir dist
# Android（SDK/NDK/Gradle/JDK 17）：
packaging/android/build.sh --output-dir dist
```

跨平台归档、Windows Inno Setup、Linux DEB/RPM、macOS PKG/DMG 和 Android APK/AAB
都在 release workflow 的相应平台 runner 上生成。签名相关秘密均为可选：

- macOS：`MAC_SIGNING_IDENTITY`；公证使用 `MAC_NOTARY_KEY_PATH`（或 workflow 中的
  `MAC_NOTARY_KEY_BASE64`）+ `MAC_NOTARY_KEY_ID` + `MAC_NOTARY_ISSUER_ID`，或
  `APPLE_ID` + `APPLE_PASSWORD` + `APPLE_TEAM_ID`。可选 `MAC_DMG_SIGNING_IDENTITY` 用于签署 DMG；
  `MAC_PRE_NOTARIZE_HOOK` / `MAC_POST_NOTARIZE_HOOK` 是可执行脚本路径，分别接收待公证和已 staple 的产物路径。
- Android：`ANDROID_KEYSTORE_BASE64`、`ANDROID_KEYSTORE_PASSWORD`、`ANDROID_KEY_ALIAS`、
  `ANDROID_KEY_PASSWORD`。本地构建四项齐全才签名 release，否则生成带 `unsigned` 标记的 APK/AAB；
  Release workflow 四项必需，缺失时明确失败。私钥、keystore 与凭据文件不得入库。

脚本、静态校验和模拟载荷测试可在任意有 Bash/Python 的机器运行；真正的安装、签名、公证和
系统服务行为必须在对应操作系统上验证。
