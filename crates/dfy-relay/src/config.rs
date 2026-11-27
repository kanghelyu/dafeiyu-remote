// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Dafeiyu Remote contributors
// Copyright (c) 2026 Dafeiyu Remote contributors
//! 配置模型：TOML 文件 → 校验后的 [`Config`]，以及 `config init` 模板生成。

use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use rand::RngCore;
use serde::Deserialize;

pub const DEFAULT_BIND: &str = "0.0.0.0:21117";
pub const DEFAULT_TCP_BIND: &str = "0.0.0.0:21118";
pub const DEFAULT_ADMIN: &str = "127.0.0.1:21119";
pub const DEFAULT_MAX_SESSIONS: usize = 1024;
pub const DEFAULT_PER_TAG_MBPS: f64 = 200.0;
pub const DEFAULT_TAG_TTL_HOURS: f64 = 48.0;
pub const DEFAULT_IDLE_TIMEOUT_S: u64 = 60;
pub const DEFAULT_BIND_WINDOW_S: u64 = 300;
pub const DEFAULT_REBIND_GUARD_MS: u64 = 2000;
/// nonce 去重表容量（防内存放大）。
const NONCE_CAPACITY: usize = 8192;

/// 32 字节中继预共享密钥。Debug 输出脱敏。
#[derive(Clone, PartialEq, Eq)]
pub struct RelayKey([u8; 32]);

impl RelayKey {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != 32 {
            bail!("relay_key 必须为 32 字节，实际 {} 字节", bytes.len());
        }
        let mut k = [0u8; 32];
        k.copy_from_slice(bytes);
        Ok(Self(k))
    }

    pub fn random() -> Self {
        let mut k = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut k);
        Self(k)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// 客户端配置写法：`base64:...`。
    pub fn to_config_string(&self) -> String {
        format!("base64:{}", B64.encode(self.0))
    }

    /// 接受 `base64:`、`hex:` 前缀或裸 64 位 hex。
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        let bytes = if let Some(rest) = s.strip_prefix("base64:") {
            B64.decode(rest.trim())
                .context("relay_key base64 解码失败")?
        } else if let Some(rest) = s.strip_prefix("hex:") {
            hex::decode(rest.trim()).context("relay_key hex 解码失败")?
        } else if s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit()) {
            hex::decode(s).context("relay_key hex 解码失败")?
        } else {
            bail!("relay_key 格式非法：需要 \"base64:<...>\"、\"hex:<...>\" 或 64 位 hex")
        };
        Self::from_bytes(&bytes)
    }
}

impl fmt::Debug for RelayKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RelayKey(redacted)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogLevel {
    Trace,
    Debug,
    #[default]
    Info,
    Warn,
    Error,
    Off,
}

impl LogLevel {
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s.trim().to_ascii_lowercase().as_str() {
            "trace" => Self::Trace,
            "debug" => Self::Debug,
            "info" => Self::Info,
            "warn" | "warning" => Self::Warn,
            "error" => Self::Error,
            "off" | "none" => Self::Off,
            other => bail!("log 级别非法：{other}（可选 trace|debug|info|warn|error|off）"),
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
            Self::Off => "off",
        }
    }
}

/// TOML 原始形态（字段与 `relay.toml` 一一对应）。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct FileConfig {
    pub bind: String,
    pub tcp_bind: Option<String>,
    pub admin_addr: String,
    pub relay_key: Option<String>,
    pub key_file: Option<PathBuf>,
    pub max_sessions: usize,
    pub per_tag_mbps: f64,
    pub max_global_mbps: Option<f64>,
    pub log: String,
    pub log_json: bool,
    pub no_log_ips: bool,
    // 高级/测试旋钮（DESIGN-NOTES 有说明）
    pub tag_ttl_hours: f64,
    pub idle_timeout_s: u64,
    pub bind_window_s: u64,
    pub rebind_guard_ms: u64,
}

impl Default for FileConfig {
    fn default() -> Self {
        Self {
            bind: DEFAULT_BIND.to_string(),
            tcp_bind: Some(DEFAULT_TCP_BIND.to_string()),
            admin_addr: DEFAULT_ADMIN.to_string(),
            relay_key: None,
            key_file: None,
            max_sessions: DEFAULT_MAX_SESSIONS,
            per_tag_mbps: DEFAULT_PER_TAG_MBPS,
            max_global_mbps: None,
            log: "info".to_string(),
            log_json: false,
            no_log_ips: true,
            tag_ttl_hours: DEFAULT_TAG_TTL_HOURS,
            idle_timeout_s: DEFAULT_IDLE_TIMEOUT_S,
            bind_window_s: DEFAULT_BIND_WINDOW_S,
            rebind_guard_ms: DEFAULT_REBIND_GUARD_MS,
        }
    }
}

/// 会话表运行期限制（state.rs 使用）。
#[derive(Debug, Clone)]
pub struct Limits {
    pub max_sessions: usize,
    pub per_tag_bytes_per_s: Option<f64>,
    pub global_bytes_per_s: Option<f64>,
    pub tag_ttl: Duration,
    pub idle_timeout: Duration,
    pub bind_window_s: u64,
    pub nonce_capacity: usize,
    pub rebind_guard: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_sessions: DEFAULT_MAX_SESSIONS,
            per_tag_bytes_per_s: Some(DEFAULT_PER_TAG_MBPS * 1_000_000.0 / 8.0),
            global_bytes_per_s: None,
            tag_ttl: Duration::from_secs(48 * 3600),
            idle_timeout: Duration::from_secs(DEFAULT_IDLE_TIMEOUT_S),
            bind_window_s: DEFAULT_BIND_WINDOW_S,
            nonce_capacity: NONCE_CAPACITY,
            rebind_guard: Duration::from_millis(DEFAULT_REBIND_GUARD_MS),
        }
    }
}

/// 校验后的运行配置。
#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub tcp_bind: Option<SocketAddr>,
    pub admin_addr: Option<SocketAddr>,
    pub key: RelayKey,
    pub log: LogLevel,
    pub log_json: bool,
    pub no_log_ips: bool,
    pub limits: Limits,
}

fn parse_socket_addr(raw: &str, field: &str) -> Result<SocketAddr> {
    raw.trim()
        .parse::<SocketAddr>()
        .with_context(|| format!("{field} 不是合法的 IP:端口：{raw}"))
}

fn optional_addr(raw: &str, field: &str) -> Result<Option<SocketAddr>> {
    let s = raw.trim();
    if s.is_empty() {
        return Ok(None);
    }
    Ok(Some(parse_socket_addr(s, field)?))
}

impl Config {
    /// 从 TOML 文件加载并校验。
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("读取配置文件失败：{}", path.display()))?;
        let base = path.parent().unwrap_or_else(|| Path::new("."));
        Self::from_toml(&text, base)
            .with_context(|| format!("配置文件校验失败：{}", path.display()))
    }

    pub fn from_toml(text: &str, base_dir: &Path) -> Result<Self> {
        let fc: FileConfig = toml::from_str(text).context("TOML 解析失败")?;
        Self::from_file_config(fc, base_dir)
    }

    pub fn from_file_config(fc: FileConfig, base_dir: &Path) -> Result<Self> {
        // 1) 密钥
        let key = match (&fc.relay_key, &fc.key_file) {
            (Some(_), Some(_)) => bail!("relay_key 与 key_file 只能二选一"),
            (Some(s), None) => {
                let s = s.trim();
                // 兼容模板里尚未替换的占位符。
                if s.is_empty() || s.contains('<') {
                    bail!("relay_key 未填写（先用 `dfy-relay config init` 生成）");
                }
                RelayKey::parse(s)?
            }
            (None, Some(p)) => {
                let p = if p.is_absolute() {
                    p.clone()
                } else {
                    base_dir.join(p)
                };
                let bytes = std::fs::read(&p)
                    .with_context(|| format!("读取 key_file 失败：{}", p.display()))?;
                if bytes.len() == 32 {
                    RelayKey::from_bytes(&bytes)?
                } else {
                    let text = String::from_utf8(bytes)
                        .context("key_file 既不是 32 字节裸数据也不是 UTF-8 文本")?;
                    RelayKey::parse(&text)?
                }
            }
            (None, None) => bail!("缺少 relay_key 或 key_file（必填）"),
        };

        // 2) 地址
        let bind = parse_socket_addr(&fc.bind, "bind")?;
        let tcp_bind = match &fc.tcp_bind {
            Some(s) => optional_addr(s, "tcp_bind")?,
            None => None,
        };
        let admin_addr = optional_addr(&fc.admin_addr, "admin_addr")?;

        // 3) 数值
        if fc.max_sessions == 0 {
            bail!("max_sessions 必须 >= 1");
        }
        if !fc.per_tag_mbps.is_finite() || fc.per_tag_mbps < 0.0 {
            bail!("per_tag_mbps 必须是 >= 0 的有限数（0 表示不限速）");
        }
        if let Some(g) = fc.max_global_mbps {
            if !g.is_finite() || g < 0.0 {
                bail!("max_global_mbps 必须是 >= 0 的有限数（0 或缺省表示不限速）");
            }
        }
        if !fc.tag_ttl_hours.is_finite() || fc.tag_ttl_hours <= 0.0 {
            bail!("tag_ttl_hours 必须 > 0");
        }
        if fc.idle_timeout_s == 0 {
            bail!("idle_timeout_s 必须 >= 1");
        }
        if fc.bind_window_s == 0 {
            bail!("bind_window_s 必须 >= 1");
        }

        let log = LogLevel::parse(&fc.log)?;
        let per_tag_bytes_per_s = if fc.per_tag_mbps > 0.0 {
            Some(fc.per_tag_mbps * 1_000_000.0 / 8.0)
        } else {
            None
        };
        let global_bytes_per_s = match fc.max_global_mbps {
            Some(g) if g > 0.0 => Some(g * 1_000_000.0 / 8.0),
            _ => None,
        };

        let cfg = Self {
            bind,
            tcp_bind,
            admin_addr,
            key,
            log,
            log_json: fc.log_json,
            no_log_ips: fc.no_log_ips,
            limits: Limits {
                max_sessions: fc.max_sessions,
                per_tag_bytes_per_s,
                global_bytes_per_s,
                tag_ttl: Duration::from_secs_f64(fc.tag_ttl_hours * 3600.0),
                idle_timeout: Duration::from_secs(fc.idle_timeout_s),
                bind_window_s: fc.bind_window_s,
                nonce_capacity: NONCE_CAPACITY,
                rebind_guard: Duration::from_millis(fc.rebind_guard_ms),
            },
        };
        cfg.validate_admin_addr()?;
        Ok(cfg)
    }

    /// 管理接口尚未实现远程鉴权，所有配置入口及实际启动都必须强制回环。
    pub fn validate_admin_addr(&self) -> Result<()> {
        if let Some(addr) = self.admin_addr {
            if !addr.ip().is_loopback() {
                bail!("admin_addr 必须为回环地址（127.0.0.0/8 或 ::1），远程管理尚未启用");
            }
        }
        Ok(())
    }

    /// 配置模板（`config init` 使用）。
    pub fn template(key: &RelayKey, bind: &str, tcp_bind: &str, admin: &str) -> String {
        format!(
            r#"# dfy-relay 中继配置（LicenseRef-PolyForm-Noncommercial-1.0.0）
# 该文件包含凭据，权限应为 0600。

# 数据面 UDP 监听地址（客户端 relay.addr 填公网 IP:21117）
bind = "{bind}"

# TCP 降级通道；留空字符串表示不启用
tcp_bind = "{tcp_bind}"

# 本机管理/指标（只监听回环，切勿改到 0.0.0.0）
admin_addr = "{admin}"

# 32 字节中继预共享密钥；客户端 relay.key 填同一个值
relay_key = "{key}"

# 并发 tag 会话上限
max_sessions = {max_sessions}

# 每 tag 限速（Mbps，0 = 不限）
per_tag_mbps = {per_tag_mbps}

# 全局带宽上限（Mbps，注释掉 = 不限）
# max_global_mbps = 1000

log = "info"        # trace|debug|info|warn|error|off
log_json = false
no_log_ips = true   # 日志不记录对端 IP（默认推荐）

# 高级旋钮（一般不用改）
tag_ttl_hours = {tag_ttl_hours}
idle_timeout_s = {idle_timeout_s}
bind_window_s = {bind_window_s}
rebind_guard_ms = {rebind_guard_ms}
"#,
            key = key.to_config_string(),
            max_sessions = DEFAULT_MAX_SESSIONS,
            per_tag_mbps = DEFAULT_PER_TAG_MBPS,
            tag_ttl_hours = DEFAULT_TAG_TTL_HOURS,
            idle_timeout_s = DEFAULT_IDLE_TIMEOUT_S,
            bind_window_s = DEFAULT_BIND_WINDOW_S,
            rebind_guard_ms = DEFAULT_REBIND_GUARD_MS,
        )
    }

    /// 客户端侧配置片段（`config init` 打印用）。
    pub fn client_snippet(addr: &str, key: &RelayKey, name: &str) -> String {
        format!(
            r#"[[relay]]
name = "{name}"
addr = "{addr}"
key = "{key}"
tls = "auto"
priority = 10
enabled = true"#,
            key = key.to_config_string()
        )
    }
}

/// 默认配置路径（可用 `DFY_RELAY_CONFIG` 覆盖）。
pub fn default_config_path() -> PathBuf {
    if let Some(p) = std::env::var_os("DFY_RELAY_CONFIG") {
        return PathBuf::from(p);
    }
    #[cfg(unix)]
    {
        PathBuf::from("/etc/dfy/relay.toml")
    }
    #[cfg(windows)]
    {
        PathBuf::from(r"relay.toml")
    }
}

/// 解析实际使用的配置文件：显式 `--config` > 默认路径 > 当前目录 `relay.toml`。
pub fn resolve_config_path(explicit: Option<PathBuf>) -> PathBuf {
    if let Some(p) = explicit {
        return p;
    }
    let def = default_config_path();
    if def.exists() {
        return def;
    }
    let local = PathBuf::from("relay.toml");
    if local.exists() {
        return local;
    }
    def
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_B64: &str = "base64:AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";

    fn minimal_toml(key: &str) -> String {
        format!(
            r#"
bind = "0.0.0.0:21117"
tcp_bind = "0.0.0.0:21118"
admin_addr = "127.0.0.1:21119"
relay_key = "{key}"
max_sessions = 64
per_tag_mbps = 10
"#
        )
    }

    #[test]
    fn key_accepts_base64_hex_and_rejects_bad() {
        let k = RelayKey::parse(KEY_B64).expect("base64 key");
        let hexed = format!("hex:{}", hex::encode(k.as_bytes()));
        assert_eq!(RelayKey::parse(&hexed).unwrap(), k);
        assert_eq!(
            RelayKey::parse(&hex::encode(k.as_bytes())).unwrap(),
            k,
            "裸 hex 也应接受"
        );
        assert!(RelayKey::parse("base64:AAAA").is_err(), "长度不足必须拒绝");
        assert!(RelayKey::parse("hello").is_err());
        assert_eq!(k.to_config_string(), KEY_B64);
    }

    #[test]
    fn full_config_parses() {
        let cfg = Config::from_toml(&minimal_toml(KEY_B64), Path::new(".")).expect("parse");
        assert_eq!(cfg.bind.port(), 21117);
        assert_eq!(cfg.tcp_bind.map(|a| a.port()), Some(21118));
        assert_eq!(cfg.admin_addr.map(|a| a.port()), Some(21119));
        assert_eq!(cfg.limits.max_sessions, 64);
        assert!((cfg.limits.per_tag_bytes_per_s.unwrap() - 10.0 * 125_000.0).abs() < 1.0);
        assert_eq!(cfg.log, LogLevel::Info);
        assert!(cfg.no_log_ips, "no_log_ips 默认 true");
    }

    #[test]
    fn missing_key_is_error() {
        let mut toml = minimal_toml(KEY_B64);
        toml = toml.replace(&format!("relay_key = \"{KEY_B64}\""), "");
        let err = Config::from_toml(&toml, Path::new(".")).unwrap_err();
        assert!(format!("{err:#}").contains("relay_key"), "{err:#}");
    }

    #[test]
    fn bad_addr_and_bad_numbers_are_errors() {
        let bad_addr = minimal_toml(KEY_B64).replace("0.0.0.0:21117", "not-an-addr");
        assert!(Config::from_toml(&bad_addr, Path::new(".")).is_err());
        let bad_sessions = minimal_toml(KEY_B64).replace("max_sessions = 64", "max_sessions = 0");
        assert!(Config::from_toml(&bad_sessions, Path::new(".")).is_err());
        let bad_log = minimal_toml(KEY_B64).replace("per_tag_mbps = 10", "log = \"loud\"");
        assert!(Config::from_toml(&bad_log, Path::new(".")).is_err());
    }

    #[test]
    fn admin_addresses_require_loopback_or_disabled() {
        for addr in ["0.0.0.0:0", "192.0.2.1:0", "[::]:0", "[2001:db8::1]:0"] {
            let text = minimal_toml(KEY_B64).replace("127.0.0.1:21119", addr);
            let err = Config::from_toml(&text, Path::new(".")).unwrap_err();
            assert!(err.to_string().contains("admin_addr"), "{addr}: {err:#}");
        }
        for addr in ["127.0.0.1:0", "127.0.0.2:0", "[::1]:0", ""] {
            let text = minimal_toml(KEY_B64).replace("127.0.0.1:21119", addr);
            assert!(Config::from_toml(&text, Path::new(".")).is_ok(), "{addr}");
        }
    }

    #[test]
    fn template_roundtrips() {
        let key = RelayKey::random();
        let text = Config::template(&key, "0.0.0.0:21117", "0.0.0.0:21118", "127.0.0.1:21119");
        let cfg = Config::from_toml(&text, Path::new(".")).expect("模板必须能被自己解析");
        assert_eq!(cfg.key, key);
        assert_eq!(cfg.bind.port(), 21117);
    }

    #[test]
    fn key_file_accepts_raw_and_text() {
        let dir = std::env::temp_dir().join(format!("dfy-relay-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = RelayKey::random();
        let raw = dir.join("raw.key");
        std::fs::write(&raw, key.as_bytes()).unwrap();
        let text = dir.join("text.key");
        std::fs::write(&text, key.to_config_string()).unwrap();

        for (field, name) in [("key_file", "raw.key"), ("key_file", "text.key")] {
            let toml =
                format!("bind = \"0.0.0.0:21117\"\nadmin_addr = \"\"\n{field} = \"{name}\"\n");
            let cfg = Config::from_toml(&toml, &dir).expect("key_file 应可加载");
            assert_eq!(cfg.key, key, "{field}={name}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
