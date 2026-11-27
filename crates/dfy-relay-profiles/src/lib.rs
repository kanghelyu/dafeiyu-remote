// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 用户自建中继池：档案管理、健康状态、选择策略与导入导出。
//!
//! 配置文件包含 `base64:` 形式的预共享密钥，必须按机密文件保护：
//! 写入时使用原子替换，并在 Unix 上设置 0600；导出时默认脱敏。

use std::fmt;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use dfy_relay::config::RelayKey;
use serde::{Deserialize, Serialize};

pub const STORE_VERSION: u32 = 1;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

/// 中继 TLS 策略。当前 relay 转发面由 QUIC/TLS 自身保护，字段用于后续外层 TLS 兼容。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TlsMode {
    /// 按服务器 advertised 能力自动选择。
    #[default]
    Auto,
    /// 禁用外层 TLS（QUIC/TLS 仍然有效）。
    Disabled,
    /// 要求外层 TLS；不支持时拒绝连接。
    Required,
}

impl TlsMode {
    pub fn parse(text: &str) -> Result<Self> {
        Ok(match text.trim().to_ascii_lowercase().as_str() {
            "auto" => Self::Auto,
            "disabled" | "off" => Self::Disabled,
            "required" | "require" => Self::Required,
            other => bail!("TLS 策略非法：{other}（可选 auto|disabled|required）"),
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Disabled => "disabled",
            Self::Required => "required",
        }
    }
}

/// 一次连通性测试的结果。仅包含非机密指标。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HealthStatus {
    pub ok: bool,
    pub rtt_ms: Option<f64>,
    /// 丢包 ppm（百万分之一）。测试通道暂无法可靠测量时为 None。
    pub loss_ppm: Option<u32>,
    /// 实测有效带宽（bit/s）。测试通道暂无法可靠测量时为 None。
    pub bandwidth_bps: Option<u64>,
    pub checked_at_ms: u64,
    pub error: Option<String>,
}

impl HealthStatus {
    pub fn success(rtt_ms: f64) -> Self {
        Self {
            ok: true,
            rtt_ms: Some(rtt_ms),
            loss_ppm: None,
            bandwidth_bps: None,
            checked_at_ms: now_ms(),
            error: None,
        }
    }

    pub fn failure(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            rtt_ms: None,
            loss_ppm: None,
            bandwidth_bps: None,
            checked_at_ms: now_ms(),
            error: Some(message.into()),
        }
    }
}

/// 一台用户中继服务器的持久档案。
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct RelayProfile {
    pub id: String,
    pub name: String,
    /// 主机地址，可以是 IP 或可解析的 DNS 名称。
    pub host: String,
    pub port: u16,
    /// `base64:` / `hex:` / 裸 hex，自动解析时由服务端工具生成。
    key_config: String,
    pub region: Option<String>,
    /// 优先级；数值越小越优先，默认 100。
    pub priority: u32,
    pub preferred: bool,
    pub tls: TlsMode,
    #[serde(default)]
    pub health: HealthStatus,
}

impl fmt::Debug for RelayProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RelayProfile")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("addr", &format_args!("{}:{}", self.host, self.port))
            .field("key_config", &"<redacted>")
            .field("region", &self.region)
            .field("priority", &self.priority)
            .field("preferred", &self.preferred)
            .field("tls", &self.tls.as_str())
            .field("health", &self.health)
            .finish()
    }
}

impl RelayProfile {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        host: impl Into<String>,
        port: u16,
        key_config: impl Into<String>,
        region: Option<String>,
        priority: u32,
        preferred: bool,
        tls: TlsMode,
    ) -> Result<Self> {
        let profile = Self {
            id: id.into(),
            name: name.into(),
            host: host.into(),
            port,
            key_config: key_config.into(),
            region,
            priority,
            preferred,
            tls,
            health: HealthStatus::default(),
        };
        profile.validate()?;
        Ok(profile)
    }

    pub fn validate(&self) -> Result<()> {
        if self.id.trim().is_empty() {
            bail!("中继 id 不能为空");
        }
        if self.id.chars().any(|c| c.is_whitespace()) {
            bail!("中继 id 不能包含空白：{}", self.id);
        }
        if self.name.trim().is_empty() {
            bail!("中继显示名称不能为空");
        }
        if self.host.trim().is_empty() {
            bail!("中继主机地址不能为空");
        }
        if self.port == 0 {
            bail!("中继端口必须大于 0");
        }
        self.key()?;
        Ok(())
    }

    pub fn addr(&self) -> Result<SocketAddr> {
        format!("{}:{}", self.host, self.port)
            .parse()
            .with_context(|| format!("中继地址非法：{}:{}", self.host, self.port))
    }

    pub fn key(&self) -> Result<RelayKey> {
        RelayKey::parse(&self.key_config)
            .with_context(|| format!("中继 {} 的密钥格式非法", self.id))
    }

    pub fn key_config(&self) -> &str {
        &self.key_config
    }

    pub fn set_key_config(&mut self, value: impl Into<String>) -> Result<()> {
        self.key_config = value.into();
        self.key()?;
        Ok(())
    }

    /// 安全展示：只暴露格式前缀，不暴露任何密钥内容。
    pub fn redacted_key(&self) -> String {
        let prefix = self
            .key_config
            .split_once(':')
            .map(|(prefix, _)| prefix)
            .unwrap_or("hex");
        format!("{prefix}:<redacted>")
    }

    pub fn endpoint(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// 选择评分：越小越优先。健康且低延迟、低丢包、低 priority 值优先。
    fn score(&self) -> f64 {
        let rtt = self.health.rtt_ms.unwrap_or(5_000.0);
        let loss = f64::from(self.health.loss_ppm.unwrap_or(u32::MAX)) / 10_000.0;
        let priority = f64::from(self.priority.min(10_000));
        rtt + loss * 500.0 + priority / 100.0
    }
}

/// 服务器选择策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionStrategy {
    /// 只用手工指定的服务器。
    Manual,
    /// 延迟最低优先。
    LowestLatency,
    /// 首选服务器优先，其次按健康评分。
    #[default]
    PreferredFirst,
    /// 自动故障切换：按优先级与健康状态选择。
    AutoFailover,
}

impl SelectionStrategy {
    pub fn parse(text: &str) -> Result<Self> {
        Ok(match text.trim().to_ascii_lowercase().as_str() {
            "manual" => Self::Manual,
            "lowest_latency" | "lowest-latency" => Self::LowestLatency,
            "preferred_first" | "preferred-first" | "preferred" => Self::PreferredFirst,
            "auto_failover" | "auto-failover" | "auto" => Self::AutoFailover,
            other => bail!(
                "选择策略非法：{other}（可选 manual|lowest_latency|preferred_first|auto_failover）"
            ),
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::LowestLatency => "lowest_latency",
            Self::PreferredFirst => "preferred_first",
            Self::AutoFailover => "auto_failover",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SelectionConfig {
    pub strategy: SelectionStrategy,
    pub active_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoreFile {
    version: u32,
    #[serde(default)]
    selection: SelectionConfig,
    #[serde(default)]
    profiles: Vec<RelayProfile>,
}

impl Default for StoreFile {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            selection: SelectionConfig::default(),
            profiles: Vec::new(),
        }
    }
}

/// 一个客户端的中继池文件。
#[derive(Debug, Clone)]
pub struct RelayProfileStore {
    path: PathBuf,
    file: StoreFile,
}

impl RelayProfileStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            file: StoreFile::default(),
        }
    }

    /// 从 TOML 加载；文件不存在时返回空池。
    pub fn load(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if !path.exists() {
            return Ok(Self::new(path));
        }
        let text = fs::read_to_string(&path)
            .with_context(|| format!("读取中继池配置失败：{}", path.display()))?;
        let file: StoreFile = toml::from_str(&text)
            .with_context(|| format!("解析中继池配置失败：{}", path.display()))?;
        if file.version != STORE_VERSION {
            bail!(
                "不支持的中继池配置版本 {}（期望 {}）",
                file.version,
                STORE_VERSION
            );
        }
        for profile in &file.profiles {
            profile
                .validate()
                .with_context(|| format!("中继档案 {} 校验失败", profile.id))?;
        }
        if let Some(active) = &file.selection.active_id {
            if !file.profiles.iter().any(|profile| &profile.id == active) {
                bail!("当前服务器指向不存在的档案：{active}");
            }
        }
        Ok(Self { path, file })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn profiles(&self) -> &[RelayProfile] {
        &self.file.profiles
    }

    pub fn selection(&self) -> &SelectionConfig {
        &self.file.selection
    }

    pub fn get(&self, id: &str) -> Option<&RelayProfile> {
        self.file.profiles.iter().find(|profile| profile.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut RelayProfile> {
        self.file
            .profiles
            .iter_mut()
            .find(|profile| profile.id == id)
    }

    pub fn add(&mut self, profile: RelayProfile) -> Result<()> {
        profile.validate()?;
        if self.get(&profile.id).is_some() {
            bail!("中继档案 id 已存在：{}", profile.id);
        }
        self.file.profiles.push(profile);
        if self.file.selection.active_id.is_none() {
            self.file.selection.active_id = self.file.profiles.first().map(|p| p.id.clone());
        }
        Ok(())
    }

    pub fn remove(&mut self, id: &str) -> Result<RelayProfile> {
        let index = self
            .file
            .profiles
            .iter()
            .position(|profile| profile.id == id)
            .with_context(|| format!("找不到中继档案：{id}"))?;
        let removed = self.file.profiles.remove(index);
        if self.file.selection.active_id.as_deref() == Some(id) {
            self.file.selection.active_id = self.file.profiles.first().map(|p| p.id.clone());
        }
        Ok(removed)
    }

    pub fn set_active(&mut self, id: &str) -> Result<()> {
        if self.get(id).is_none() {
            bail!("找不到中继档案：{id}");
        }
        self.file.selection.active_id = Some(id.to_string());
        self.file.selection.strategy = SelectionStrategy::Manual;
        Ok(())
    }

    pub fn set_strategy(&mut self, strategy: SelectionStrategy) {
        self.file.selection.strategy = strategy;
    }

    pub fn update_health(&mut self, id: &str, health: HealthStatus) -> Result<()> {
        let profile = self
            .get_mut(id)
            .with_context(|| format!("找不到中继档案：{id}"))?;
        profile.health = health;
        Ok(())
    }

    /// 按当前策略选择服务器。自动策略在没有健康样本时退化为按优先级/名称稳定排序。
    pub fn selected(&self) -> Option<&RelayProfile> {
        match self.file.selection.strategy {
            SelectionStrategy::Manual => self
                .file
                .selection
                .active_id
                .as_deref()
                .and_then(|id| self.get(id))
                .or_else(|| self.file.profiles.first()),
            SelectionStrategy::PreferredFirst => Self::best_preferred(&self.file.profiles),
            SelectionStrategy::LowestLatency => Self::best(&self.file.profiles, |p| p.health.ok),
            SelectionStrategy::AutoFailover => Self::best(&self.file.profiles, |_| true),
        }
    }

    /// 当前失败后选择下一台，避免立刻重试同一台。
    pub fn next_after_failure(&self, failed_id: &str) -> Option<&RelayProfile> {
        self.file
            .profiles
            .iter()
            .filter(|profile| profile.id != failed_id && profile.health.ok)
            .min_by(|a, b| {
                a.priority
                    .cmp(&b.priority)
                    .then(a.score().total_cmp(&b.score()))
                    .then(a.id.cmp(&b.id))
            })
            .or_else(|| {
                self.file
                    .profiles
                    .iter()
                    .find(|profile| profile.id != failed_id)
            })
    }

    /// 控制台显示当前服务器，便于用户明确知道流量经过哪台中继。
    pub fn current_display(&self) -> Option<String> {
        self.selected()
            .map(|profile| format!("{} ({})", profile.name, profile.endpoint()))
    }

    fn best_preferred(profiles: &[RelayProfile]) -> Option<&RelayProfile> {
        profiles
            .iter()
            .min_by(|a, b| {
                b.preferred
                    .cmp(&a.preferred)
                    .then(a.priority.cmp(&b.priority))
                    .then(a.score().total_cmp(&b.score()))
                    .then(a.id.cmp(&b.id))
            })
            .or_else(|| profiles.first())
    }

    fn best<F>(profiles: &[RelayProfile], healthy: F) -> Option<&RelayProfile>
    where
        F: Fn(&RelayProfile) -> bool,
    {
        profiles
            .iter()
            .filter(|profile| healthy(profile))
            .min_by(|a, b| {
                a.priority
                    .cmp(&b.priority)
                    .then(a.score().total_cmp(&b.score()))
                    .then(a.id.cmp(&b.id))
            })
            .or_else(|| profiles.first())
    }

    /// 原子写入；Unix 上设置 0600。
    pub fn save(&self) -> Result<()> {
        for profile in &self.file.profiles {
            profile.validate()?;
        }
        let parent = self
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)
            .with_context(|| format!("创建配置目录失败：{}", parent.display()))?;
        let temp = self
            .path
            .with_extension(format!("tmp.{}", std::process::id()));
        let text = toml::to_string_pretty(&self.file).context("序列化中继池配置失败")?;
        fs::write(&temp, text).with_context(|| format!("写入临时配置失败：{}", temp.display()))?;
        restrict_private_file(&temp)?;
        fs::rename(&temp, &self.path).with_context(|| {
            format!(
                "原子替换中继池配置失败：{} -> {}",
                temp.display(),
                self.path.display()
            )
        })?;
        restrict_private_file(&self.path)?;
        Ok(())
    }

    /// 导入另一份配置。`merge=false` 时完全覆盖当前池。
    pub fn import(&mut self, other: &RelayProfileStore, merge: bool) -> Result<usize> {
        if !merge {
            self.file = other.file.clone();
            return Ok(self.file.profiles.len());
        }
        let mut added = 0;
        for profile in &other.file.profiles {
            if self.get(&profile.id).is_none() {
                self.file.profiles.push(profile.clone());
                added += 1;
            }
        }
        if self.file.selection.active_id.is_none() {
            self.file.selection.active_id = other.file.selection.active_id.clone();
        }
        Ok(added)
    }

    /// 导出为可写出的 TOML。`redact=true` 时替换密钥，不能用于直接导入。
    pub fn export_toml(&self, redact: bool) -> Result<String> {
        let mut file = self.file.clone();
        if redact {
            for profile in &mut file.profiles {
                profile.key_config = "<redacted>".to_string();
            }
        }
        toml::to_string_pretty(&file).context("导出中继池配置失败")
    }

    /// 导出到文件，并按机密文件权限写入。
    pub fn export_to_file(&self, path: &Path, redact: bool) -> Result<()> {
        let text = self.export_toml(redact)?;
        let parent = path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)
            .with_context(|| format!("创建导出目录失败：{}", parent.display()))?;
        fs::write(path, text).with_context(|| format!("写入导出文件失败：{}", path.display()))?;
        restrict_private_file(path)?;
        Ok(())
    }
}

/// 限制文件只能由当前用户读写。Windows 使用用户目录/ACL 由部署流程负责。
fn restrict_private_file(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("设置配置权限 0600 失败：{}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> String {
        format!("hex:{}", "11".repeat(32))
    }

    fn profile(id: &str, name: &str, rtt: Option<f64>) -> RelayProfile {
        let mut p = RelayProfile::new(
            id,
            name,
            "127.0.0.1",
            21117,
            key(),
            None,
            100,
            false,
            TlsMode::Auto,
        )
        .unwrap();
        if let Some(rtt) = rtt {
            p.health = HealthStatus::success(rtt);
        }
        p
    }

    #[test]
    fn crud_and_validation() {
        let mut store = RelayProfileStore::new("unused.toml");
        assert!(RelayProfile::new(
            "",
            "bad",
            "127.0.0.1",
            21117,
            key(),
            None,
            1,
            false,
            TlsMode::Auto
        )
        .is_err());
        store.add(profile("a", "A", None)).unwrap();
        assert!(store.add(profile("a", "dup", None)).is_err());
        store.add(profile("b", "B", Some(10.0))).unwrap();
        assert_eq!(store.profiles().len(), 2);
        store.remove("a").unwrap();
        assert!(store.get("a").is_none());
        assert_eq!(
            store.current_display().as_deref(),
            Some("B (127.0.0.1:21117)")
        );
    }

    #[test]
    fn strategies_select_expected_profile() {
        let mut store = RelayProfileStore::new("unused.toml");
        store.add(profile("slow", "slow", Some(80.0))).unwrap();
        let mut fast = profile("fast", "fast", Some(12.0));
        fast.preferred = true;
        store.add(fast).unwrap();
        store.set_strategy(SelectionStrategy::LowestLatency);
        assert_eq!(store.selected().unwrap().id, "fast");
        store.set_strategy(SelectionStrategy::PreferredFirst);
        assert_eq!(store.selected().unwrap().id, "fast");
        store.set_strategy(SelectionStrategy::Manual);
        store.set_active("slow").unwrap();
        assert_eq!(store.selected().unwrap().id, "slow");
        assert_eq!(store.next_after_failure("slow").unwrap().id, "fast");
    }

    #[test]
    fn export_import_roundtrip_and_redaction() {
        let mut store = RelayProfileStore::new("unused.toml");
        store.add(profile("a", "A", Some(20.0))).unwrap();
        set_active(&mut store, "a");
        let full = store.export_toml(false).unwrap();
        let imported = parse_store(&full).unwrap();
        assert_eq!(imported.profiles()[0].key_config(), key());
        let redacted = store.export_toml(true).unwrap();
        assert!(redacted.contains("<redacted>"));
        assert!(!redacted.contains(&key()));
    }

    #[test]
    fn debug_never_prints_key() {
        let p = profile("a", "A", None);
        let text = format!("{p:?}");
        assert!(text.contains("<redacted>"));
        assert!(!text.contains(&key()));
    }

    #[cfg(unix)]
    #[test]
    fn save_sets_private_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir =
            std::env::temp_dir().join(format!("dfy-relay-profiles-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("profiles.toml");
        let mut store = RelayProfileStore::new(&path);
        store.add(profile("a", "A", None)).unwrap();
        store.save().unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        fs::remove_dir_all(&dir).unwrap();
    }

    fn set_active(store: &mut RelayProfileStore, id: &str) {
        store.set_active(id).unwrap();
    }

    fn parse_store(text: &str) -> Result<RelayProfileStore> {
        let file: StoreFile = toml::from_str(text)?;
        Ok(RelayProfileStore {
            path: PathBuf::from("memory.toml"),
            file,
        })
    }
}
