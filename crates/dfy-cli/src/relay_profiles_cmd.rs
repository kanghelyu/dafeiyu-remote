// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! `dfy relay-profile` 命令：用户中继池的增删改查、健康测试、选择与导入导出。

use std::io::ErrorKind;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use dfy_relay::config::RelayKey;
use dfy_relay::{bind_relay_socket_with, RelayClientConfig, RelaySocketOptions, Role};
use dfy_relay_profiles::{
    HealthStatus, RelayProfile, RelayProfileStore, SelectionStrategy, TlsMode,
};
use rand::RngCore;

#[derive(Debug, Subcommand)]
pub enum RelayProfileAction {
    /// 列出全部中继档案与当前选择
    List,
    /// 添加中继档案
    Add {
        #[arg(long)]
        id: String,
        #[arg(long)]
        name: String,
        /// 地址写法：host:port
        #[arg(long, value_name = "HOST:PORT")]
        addr: String,
        /// 不提供时自动随机生成并打印一次
        #[arg(long)]
        key: Option<String>,
        #[arg(long)]
        region: Option<String>,
        #[arg(long, default_value_t = 100)]
        priority: u32,
        #[arg(long)]
        preferred: bool,
        #[arg(long, value_name = "auto|disabled|required", default_value = "auto")]
        tls: String,
    },
    /// 修改中继档案
    Update {
        #[arg(long)]
        id: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long, value_name = "HOST:PORT")]
        addr: Option<String>,
        #[arg(long)]
        key: Option<String>,
        #[arg(long)]
        region: Option<String>,
        #[arg(long)]
        clear_region: bool,
        #[arg(long)]
        priority: Option<u32>,
        #[arg(long, conflicts_with = "no_preferred")]
        preferred: bool,
        #[arg(long)]
        no_preferred: bool,
        #[arg(long, value_name = "auto|disabled|required")]
        tls: Option<String>,
    },
    /// 删除中继档案
    Remove {
        #[arg(long)]
        id: String,
    },
    /// 手工指定当前服务器（同时切换到 manual 策略）
    Use {
        #[arg(long)]
        id: String,
    },
    /// 设置自动选择策略
    Select {
        #[arg(
            long,
            value_name = "manual|lowest_latency|preferred_first|auto_failover"
        )]
        strategy: String,
    },
    /// 测试一台中继的 BindRequest/BindOk 往返延迟
    Test {
        #[arg(long)]
        id: String,
        #[arg(long, default_value_t = 3000)]
        timeout_ms: u64,
    },
    /// 测试全部中继
    TestAll {
        #[arg(long, default_value_t = 3000)]
        timeout_ms: u64,
    },
    /// 从另一份配置导入；默认合并
    Import {
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        replace: bool,
    },
    /// 导出配置到文件；默认脱敏，--with-secrets 才会写出密钥
    Export {
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        with_secrets: bool,
    },
}

fn parse_addr(text: &str) -> Result<(String, u16)> {
    let (host, port) = text
        .rsplit_once(':')
        .with_context(|| format!("地址必须是 host:port：{text}"))?;
    if host.is_empty() {
        bail!("地址主机名不能为空：{text}");
    }
    let port: u16 = port.parse().with_context(|| format!("端口非法：{port}"))?;
    if port == 0 {
        bail!("端口必须大于 0");
    }
    Ok((host.to_string(), port))
}

async fn resolve_addr(profile: &RelayProfile) -> Result<SocketAddr> {
    if let Ok(addr) = profile.addr() {
        return Ok(addr);
    }
    let lookup = format!("{}:{}", profile.host, profile.port);
    let mut addresses = tokio::net::lookup_host(&lookup)
        .await
        .with_context(|| format!("解析中继主机失败：{}", profile.host))?;
    addresses
        .next()
        .with_context(|| format!("中继主机没有可用地址：{}", profile.host))
}

fn print_profile(profile: &RelayProfile, current: bool) {
    let health = if profile.health.ok {
        format!(
            "ok rtt={:?}ms loss={:?}ppm bps={:?} checked={}",
            profile.health.rtt_ms,
            profile.health.loss_ppm,
            profile.health.bandwidth_bps,
            profile.health.checked_at_ms
        )
    } else if profile.health.checked_at_ms == 0 {
        "untested".to_string()
    } else {
        format!(
            "failed checked={} error={}",
            profile.health.checked_at_ms,
            profile.health.error.as_deref().unwrap_or("unknown")
        )
    };
    println!(
        "id={} name={} addr={} region={:?} priority={} preferred={} tls={} key={} health={} current={}",
        profile.id,
        profile.name,
        profile.endpoint(),
        profile.region,
        profile.priority,
        profile.preferred,
        profile.tls.as_str(),
        profile.redacted_key(),
        health,
        current
    );
}

/// 探测包标记：与中继 BindOk/QUIC 报文区分，避免把保活回包算成数据。
const PROBE_MAGIC: [u8; 4] = [0xDF, 0x59, 0x50, 0x52];
const PROBE_PACKETS: usize = 8;
const PROBE_BYTES: usize = 1200;

/// 配对 + 双向数据面探测：不建立 QUIC 会话。
///
/// 先用一对临时 tag 的 Client/Agent socket 完成 bind 得到配对 RTT，再经中继发送
/// 固定数量、固定大小的探测包，由对端 socket 计数，算出丢包率与实际吞吐。
/// 探测不触碰用户会话：tag 每次随机生成，退出时 drop 自动停止保活。
async fn test_one(profile: &RelayProfile, timeout_ms: u64) -> Result<HealthStatus> {
    let addr = resolve_addr(profile).await?;
    let key = profile.key()?;
    let mut tag = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut tag);
    let options = RelaySocketOptions {
        bind_timeout: Duration::from_millis(timeout_ms.max(1)),
        retry_interval: Duration::from_millis(200),
        keepalive_interval: Duration::from_secs(3600),
    };
    let started = Instant::now();
    let config_client = RelayClientConfig {
        relay_addr: addr,
        tag,
        role: Role::Client,
        key: *key.as_bytes(),
        nonce: dfy_transport::now_us(),
    };
    let client = bind_relay_socket_with(&config_client, &options)
        .await
        .with_context(|| format!("与中继 {} 配对失败", profile.endpoint()))?;
    let rtt = started.elapsed().as_secs_f64() * 1000.0;

    let config_agent = RelayClientConfig {
        role: Role::Agent,
        nonce: dfy_transport::now_us().wrapping_add(1),
        ..config_client
    };
    let agent = bind_relay_socket_with(&config_agent, &options)
        .await
        .with_context(|| format!("与中继 {} 配对对端失败", profile.endpoint()))?;

    let client_addr = client.peer_addr();
    let probe = tokio::task::spawn_blocking(move || -> Result<(u64, u64)> {
        let mut payload = vec![0u8; PROBE_BYTES];
        payload[..4].copy_from_slice(&PROBE_MAGIC);
        let send_started = Instant::now();
        for i in 0..PROBE_PACKETS {
            payload[4] = i as u8;
            client
                .socket()
                .send_to(&payload, client_addr)
                .context("发送中继探测包失败")?;
            std::thread::sleep(Duration::from_millis(2));
        }
        let send_elapsed = send_started.elapsed();
        let mut received = 0usize;
        let mut buf = [0u8; PROBE_BYTES + 64];
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(500));
        while Instant::now() < deadline && received < PROBE_PACKETS {
            match agent.socket().recv(&mut buf) {
                Ok(n) if n >= 4 && buf[..4] == PROBE_MAGIC => received += 1,
                Ok(_) => {}
                Err(err)
                    if err.kind() == ErrorKind::WouldBlock || err.kind() == ErrorKind::TimedOut =>
                {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(err) => return Err(err).context("接收中继探测包失败"),
            }
        }
        let lost = (PROBE_PACKETS - received) as u64;
        let bytes = (received * PROBE_BYTES) as u64;
        let elapsed = send_elapsed.max(Duration::from_millis(1));
        let bps = bytes
            .saturating_mul(8)
            .checked_div(elapsed.as_millis().max(1) as u64)
            .unwrap_or(0)
            .saturating_mul(1000);
        Ok((lost, bps))
    })
    .await
    .context("中继探测任务失败")??;

    let mut health = HealthStatus::success(rtt);
    let (lost, bps) = probe;
    health.loss_ppm = Some(
        lost.saturating_mul(1_000_000)
            .checked_div(PROBE_PACKETS as u64)
            .unwrap_or(0)
            .min(u64::from(u32::MAX)) as u32,
    );
    health.bandwidth_bps = Some(bps);
    Ok(health)
}

pub async fn run(config: &Path, action: RelayProfileAction) -> Result<()> {
    let mut store = RelayProfileStore::load(config)?;
    match action {
        RelayProfileAction::List => {
            let current_id = store.selected().map(|profile| profile.id.clone());
            println!(
                "strategy={} current={}",
                store.selection().strategy.as_str(),
                store
                    .current_display()
                    .unwrap_or_else(|| "none".to_string())
            );
            for profile in store.profiles() {
                print_profile(profile, current_id.as_deref() == Some(&profile.id));
            }
        }
        RelayProfileAction::Add {
            id,
            name,
            addr,
            key,
            region,
            priority,
            preferred,
            tls,
        } => {
            let (host, port) = parse_addr(&addr)?;
            let key = match key {
                Some(value) => value,
                None => {
                    let generated = RelayKey::random().to_config_string();
                    println!("已为该中继生成新密钥：{generated}");
                    generated
                }
            };
            let profile = RelayProfile::new(
                id.clone(),
                name,
                host,
                port,
                key,
                region,
                priority,
                preferred,
                TlsMode::parse(&tls)?,
            )?;
            store.add(profile)?;
            store.save()?;
            println!("已添加中继 {id}，配置：{}", config.display());
            println!("注意：配置含预共享密钥，请按机密文件保护（Unix 已设 0600）。");
        }
        RelayProfileAction::Update {
            id,
            name,
            addr,
            key,
            region,
            clear_region,
            priority,
            preferred,
            no_preferred,
            tls,
        } => {
            let profile = store
                .get_mut(&id)
                .with_context(|| format!("找不到中继档案：{id}"))?;
            if let Some(name) = name {
                profile.name = name;
            }
            if let Some(addr) = addr {
                let (host, port) = parse_addr(&addr)?;
                profile.host = host;
                profile.port = port;
            }
            if let Some(key) = key {
                profile.set_key_config(key)?;
            }
            if clear_region {
                profile.region = None;
            } else if let Some(region) = region {
                profile.region = Some(region);
            }
            if let Some(priority) = priority {
                profile.priority = priority;
            }
            if preferred {
                profile.preferred = true;
            }
            if no_preferred {
                profile.preferred = false;
            }
            if let Some(tls) = tls {
                profile.tls = TlsMode::parse(&tls)?;
            }
            profile.validate()?;
            store.save()?;
            println!("已更新中继 {id}");
        }
        RelayProfileAction::Remove { id } => {
            store.remove(&id)?;
            store.save()?;
            println!("已删除中继 {id}");
        }
        RelayProfileAction::Use { id } => {
            store.set_active(&id)?;
            store.save()?;
            println!("当前中继：{}", store.current_display().unwrap_or_default());
        }
        RelayProfileAction::Select { strategy } => {
            let strategy = SelectionStrategy::parse(&strategy)?;
            store.set_strategy(strategy);
            store.save()?;
            println!("选择策略：{}", strategy.as_str());
            println!(
                "当前中继：{}",
                store.current_display().unwrap_or_else(|| "none".into())
            );
        }
        RelayProfileAction::Test { id, timeout_ms } => {
            let profile = store
                .get(&id)
                .with_context(|| format!("找不到中继档案：{id}"))?
                .clone();
            match test_one(&profile, timeout_ms).await {
                Ok(health) => {
                    println!(
                        "中继 {id} 可达：配对 {:.2}ms，丢包 {:?}ppm，带宽 {:?}bps（{PROBE_PACKETS}×{PROBE_BYTES}B 探测）",
                        health.rtt_ms.unwrap_or_default(),
                        health.loss_ppm,
                        health.bandwidth_bps
                    );
                    store.update_health(&id, health)?;
                }
                Err(err) => {
                    store.update_health(&id, HealthStatus::failure(format!("{err:#}")))?;
                    store.save()?;
                    return Err(err);
                }
            }
            store.save()?;
        }
        RelayProfileAction::TestAll { timeout_ms } => {
            let ids = store
                .profiles()
                .iter()
                .map(|profile| profile.id.clone())
                .collect::<Vec<_>>();
            let mut failures = 0;
            for id in ids {
                let profile = store.get(&id).expect("id came from store").clone();
                match test_one(&profile, timeout_ms).await {
                    Ok(health) => {
                        println!(
                            "{id}: ok {:.2}ms loss={:?}ppm bps={:?}",
                            health.rtt_ms.unwrap_or_default(),
                            health.loss_ppm,
                            health.bandwidth_bps
                        );
                        store.update_health(&id, health)?;
                    }
                    Err(err) => {
                        failures += 1;
                        println!("{id}: failed {err:#}");
                        store.update_health(&id, HealthStatus::failure(format!("{err:#}")))?;
                    }
                }
            }
            store.save()?;
            if failures > 0 {
                bail!("{failures} 台中继测试失败");
            }
        }
        RelayProfileAction::Import { file, replace } => {
            let other = RelayProfileStore::load(&file)?;
            let count = store.import(&other, !replace)?;
            store.save()?;
            println!("已导入 {count} 个中继档案（replace={replace}）");
        }
        RelayProfileAction::Export { file, with_secrets } => {
            if with_secrets {
                store.export_to_file(&file, false)?;
                println!("已导出含密钥配置：{}（按机密文件保护）", file.display());
            } else {
                store.export_to_file(&file, true)?;
                println!("已导出脱敏配置：{}", file.display());
            }
        }
    }
    Ok(())
}
