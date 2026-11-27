// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Dafeiyu Remote contributors
// Copyright (c) 2026 Dafeiyu Remote contributors
//! 命令行入口：`serve`（默认）/ `config init|check` / `status` / `doctor` / `version` / `selftest`。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use dfy_relay::config::{self, Config, RelayKey};
use dfy_relay::{admin, relay, selftest};

#[derive(Parser)]
#[command(
    name = "dfy-relay",
    version,
    about = "DFYP/1 中继服务器（大肥鱼远程）：成对 UDP 转发 / TCP 降级通道 / 本机指标"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// 启动中继服务（默认子命令）
    Serve(ServeArgs),
    /// 生成或校验配置
    Config(ConfigCmd),
    /// 查询本机 admin 运行状态
    Status(StatusArgs),
    /// 部署环境自检：端口占用 / 绑定权限 / admin 可达
    Doctor(DoctorArgs),
    /// 打印版本与协议信息
    Version,
    /// 进程内端到端自测
    Selftest,
}

#[derive(Args)]
struct ServeArgs {
    /// 配置文件路径（默认 /etc/dfy/relay.toml，可用 DFY_RELAY_CONFIG 覆盖）
    #[arg(short, long)]
    config: Option<PathBuf>,
}

#[derive(Args)]
struct ConfigCmd {
    #[command(subcommand)]
    sub: ConfigSub,
}

#[derive(Subcommand)]
enum ConfigSub {
    /// 生成含随机 relay_key 的配置模板，并打印客户端所需字段
    Init(InitArgs),
    /// 校验配置文件（TOML / key / 端口 / admin 回环）
    Check(CheckArgs),
}

#[derive(Args)]
struct InitArgs {
    /// 输出路径（默认 /etc/dfy/relay.toml 或 DFY_RELAY_CONFIG）
    #[arg(short, long)]
    out: Option<PathBuf>,
    /// 覆盖已存在的文件
    #[arg(long)]
    force: bool,
    /// UDP 数据面监听地址（示例：0.0.0.0:21117）
    #[arg(long)]
    bind: Option<String>,
    /// TCP 降级通道监听地址
    #[arg(long)]
    tcp_bind: Option<String>,
    /// 本机管理面监听地址（必须回环）
    #[arg(long)]
    admin: Option<String>,
    /// 不启用 TCP 降级通道
    #[arg(long)]
    no_tcp: bool,
}

#[derive(Args)]
struct CheckArgs {
    #[arg(short, long)]
    config: Option<PathBuf>,
}

#[derive(Args)]
struct StatusArgs {
    /// 直接指定 admin 地址（默认取配置里的 admin_addr）
    #[arg(long)]
    admin: Option<String>,
    #[arg(short, long)]
    config: Option<PathBuf>,
}

#[derive(Args)]
struct DoctorArgs {
    #[arg(short, long)]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd.unwrap_or(Cmd::Serve(ServeArgs { config: None })) {
        Cmd::Serve(args) => serve(args).await,
        Cmd::Config(c) => match c.sub {
            ConfigSub::Init(a) => config_init(a),
            ConfigSub::Check(a) => config_check(a),
        },
        Cmd::Status(a) => status(a).await,
        Cmd::Doctor(a) => doctor(a).await,
        Cmd::Version => {
            print_version();
            Ok(())
        }
        Cmd::Selftest => {
            println!("dfy-relay selftest v{}", env!("CARGO_PKG_VERSION"));
            selftest::run().await
        }
    }
}

fn print_version() {
    println!("dfy-relay {}", env!("CARGO_PKG_VERSION"));
    println!("协议：DFYP/1 —— UDP 数据面 + TCP 降级通道（可桥接），tag 16B，HMAC-SHA256 鉴权");
    println!("默认端口：21117/udp 数据，21118/tcp 降级，127.0.0.1:21119 本机管理");
    println!("许可：LicenseRef-PolyForm-Noncommercial-1.0.0");
}

fn init_tracing(cfg: &Config) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(cfg.log.as_str()));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false);
    if cfg.log_json {
        builder.json().init();
    } else {
        builder.init();
    }
}

async fn serve(args: ServeArgs) -> Result<()> {
    let path = config::resolve_config_path(args.config);
    let cfg = Config::load(&path).with_context(|| {
        format!(
            "加载配置失败：{}（先执行 dfy-relay config init）",
            path.display()
        )
    })?;
    init_tracing(&cfg);
    let cfg = Arc::new(cfg);
    let handle = relay::start(cfg.clone()).await?;

    println!("dfy-relay {} 已启动", env!("CARGO_PKG_VERSION"));
    println!("  UDP 数据面   : {}", handle.udp_local);
    match handle.tcp_local {
        Some(a) => println!("  TCP 降级通道 : {a}"),
        None => println!("  TCP 降级通道 : 未启用"),
    }
    match handle.admin_local {
        Some(a) => println!("  管理/指标    : http://{a}/metrics"),
        None => println!("  管理/指标    : 未启用"),
    }
    println!(
        "  限速         : per-tag {:?} B/s，全局 {:?} B/s，max_sessions {}",
        cfg.limits.per_tag_bytes_per_s, cfg.limits.global_bytes_per_s, cfg.limits.max_sessions
    );
    println!("  配置文件     : {}", path.display());

    wait_for_shutdown().await;
    println!("收到退出信号，正在优雅关闭...");
    handle.stop().await;
    Ok(())
}

#[cfg(unix)]
async fn wait_for_shutdown() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    let mut int = match signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(_) => {
            tokio::select! {
                _ = term.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
            return;
        }
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

#[cfg(windows)]
async fn wait_for_shutdown() {
    // Windows 下 tokio 的 ctrl_c 覆盖 Ctrl+C / Ctrl+Break。
    let _ = tokio::signal::ctrl_c().await;
}

fn config_init(args: InitArgs) -> Result<()> {
    let path = args.out.clone().unwrap_or_else(config::default_config_path);
    if path.exists() && !args.force {
        bail!("配置文件已存在：{}（加 --force 覆盖）", path.display());
    }
    let key = RelayKey::random();
    let bind = args
        .bind
        .unwrap_or_else(|| config::DEFAULT_BIND.to_string());
    let tcp_bind = match args.no_tcp {
        true => String::new(),
        false => args
            .tcp_bind
            .unwrap_or_else(|| config::DEFAULT_TCP_BIND.to_string()),
    };
    let admin = args
        .admin
        .unwrap_or_else(|| config::DEFAULT_ADMIN.to_string());
    let text = Config::template(&key, &bind, &tcp_bind, &admin);

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建目录失败：{}", parent.display()))?;
        }
    }
    std::fs::write(&path, text.as_bytes())
        .with_context(|| format!("写入配置失败：{}", path.display()))?;
    restrict_permissions(&path)?;

    println!("已生成配置：{}", path.display());
    println!("  relay_key : {}", key.to_config_string());
    println!();
    println!("客户端（dfy-cli / 客户端配置）填写：");
    println!("{}", Config::client_snippet(&bind, &key, "relay-default"));
    if bind.starts_with("0.0.0.0") {
        println!("  注意：bind 用了 0.0.0.0，客户端 addr 请替换为本机公网/内网可达 IP。");
    }
    println!();
    println!("启动：dfy-relay serve --config {}", path.display());
    Ok(())
}

#[cfg(unix)]
fn restrict_permissions(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &std::path::Path) -> Result<()> {
    // Windows ACL 继承目录权限；配置文件含 relay_key，建议放在受控目录。
    Ok(())
}

fn config_check(args: CheckArgs) -> Result<()> {
    let path = config::resolve_config_path(args.config);
    println!("检查配置：{}", path.display());
    let cfg = Config::load(&path).with_context(|| format!("配置校验失败：{}", path.display()))?;
    println!("  [OK] TOML 解析与字段校验通过");
    let key_str = cfg.key.to_config_string();
    println!(
        "  [OK] relay_key 32 字节（{}…）",
        &key_str[..key_str.len().min(22)]
    );
    println!("  [OK] bind         = {}（UDP 数据面）", cfg.bind);
    match cfg.tcp_bind {
        Some(a) => println!("  [OK] tcp_bind     = {a}（TCP 降级通道）"),
        None => println!("  [OK] tcp_bind     = 未启用"),
    }
    match cfg.admin_addr {
        Some(a) => {
            if !a.ip().is_loopback() {
                bail!("admin_addr {a} 不是回环地址：管理面会暴露到外网，必须改为 127.0.0.1 或 ::1");
            }
            println!("  [OK] admin_addr   = {a}（仅本机）");
        }
        None => println!("  [WARN] admin_addr 未配置，/healthz 与 /metrics 不可用"),
    }
    println!(
        "  [OK] max_sessions = {}，per_tag_mbps = {}，tag_ttl = {:?}，idle = {:?}",
        cfg.limits.max_sessions,
        cfg.limits
            .per_tag_bytes_per_s
            .map(|b| format!("{:.3}", b / 125_000.0))
            .unwrap_or_else(|| "不限".to_string()),
        cfg.limits.tag_ttl,
        cfg.limits.idle_timeout,
    );
    println!(
        "  [OK] log = {}，log_json = {}，no_log_ips = {}",
        cfg.log.as_str(),
        cfg.log_json,
        cfg.no_log_ips
    );
    println!("配置校验通过");
    Ok(())
}

async fn status(args: StatusArgs) -> Result<()> {
    let addr: SocketAddr = match args.admin {
        Some(s) => s
            .parse()
            .context("--admin 地址非法，例如 127.0.0.1:21119")?,
        None => {
            let path = config::resolve_config_path(args.config);
            let cfg = Config::load(&path).with_context(|| {
                format!(
                    "加载配置失败：{}（也可用 --admin 127.0.0.1:21119 直接查询）",
                    path.display()
                )
            })?;
            cfg.admin_addr
                .ok_or_else(|| anyhow!("配置未启用 admin_addr，无法查询状态"))?
        }
    };
    let body = admin::fetch_status(addr)
        .await
        .with_context(|| format!("查询 admin {addr} 失败（relay 未运行？）"))?;
    println!("{}", body.trim_end());
    Ok(())
}

async fn doctor(args: DoctorArgs) -> Result<()> {
    println!("dfy-relay doctor v{}", env!("CARGO_PKG_VERSION"));
    let path = config::resolve_config_path(args.config);
    println!("  配置文件：{}", path.display());
    let cfg = Config::load(&path).with_context(|| {
        format!(
            "配置加载失败：{}（先执行 config init / config check）",
            path.display()
        )
    })?;
    println!(
        "  [OK] 配置可解析（key 32B，log={}，no_log_ips={}）",
        cfg.log.as_str(),
        cfg.no_log_ips
    );

    match cfg.admin_addr {
        Some(a) if !a.ip().is_loopback() => {
            println!("  [ERR] admin_addr {a} 不是回环地址：管理面绝不能对外")
        }
        Some(a) => println!("  [OK] admin_addr {a} 为回环地址"),
        None => println!("  [WARN] 未配置 admin_addr：/healthz、/metrics 不可用"),
    }

    probe_udp(cfg.bind).await;
    match cfg.tcp_bind {
        Some(a) => probe_tcp(a).await,
        None => println!("  [SKIP] TCP 降级通道未启用"),
    }
    if let Some(a) = cfg.admin_addr {
        probe_admin(a, &cfg).await;
    }

    println!();
    println!("提示：");
    println!("  Linux 防火墙需放行数据面：sudo ufw allow 21117/udp && sudo ufw allow 21118/tcp");
    println!(
        "  管理面 21119 仅监听回环，不要对外放行；systemd/Docker 部署见 crates/dfy-relay/README.md"
    );
    Ok(())
}

async fn probe_udp(addr: SocketAddr) {
    match tokio::net::UdpSocket::bind(addr).await {
        Ok(sock) => {
            let local = sock.local_addr().ok();
            drop(sock);
            println!("  [OK] UDP 可绑定 {addr}（local={local:?}）");
        }
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            println!("  [WARN] UDP {addr} 已被占用（relay 可能已在运行）");
        }
        Err(e) => println!(
            "  [ERR] UDP {addr} 绑定失败：{e}（<1024 端口需要 root 或 CAP_NET_BIND_SERVICE）"
        ),
    }
}

async fn probe_tcp(addr: SocketAddr) {
    match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => {
            let local = listener.local_addr().ok();
            drop(listener);
            println!("  [OK] TCP 可绑定 {addr}（local={local:?}）");
        }
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            println!("  [WARN] TCP {addr} 已被占用（relay 可能已在运行）");
        }
        Err(e) => println!("  [ERR] TCP {addr} 绑定失败：{e}"),
    }
}

async fn probe_admin(addr: SocketAddr, cfg: &Config) {
    match tokio::time::timeout(Duration::from_secs(1), tokio::net::TcpStream::connect(addr)).await {
        Ok(Ok(mut stream)) => {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let req = format!("GET /healthz HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
            if stream.write_all(req.as_bytes()).await.is_ok() {
                let mut resp = Vec::new();
                let _ = stream.read_to_end(&mut resp).await;
                let text = String::from_utf8_lossy(&resp);
                if text.contains("200") && text.contains("ok") {
                    println!("  [OK] admin {addr} 可达且 /healthz 正常（本机已有 relay 在运行）");
                    return;
                }
            }
            println!("  [WARN] admin {addr} 端口可连，但 /healthz 响应异常（可能被其他进程占用）");
        }
        _ => println!(
            "  [OK] admin {addr} 当前未被占用（relay 未运行；仅监听 {} 侧）",
            if cfg
                .admin_addr
                .map(|a| a.ip().is_loopback())
                .unwrap_or(false)
            {
                "回环"
            } else {
                "非回环"
            }
        ),
    }
}
