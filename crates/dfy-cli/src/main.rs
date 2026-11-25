//! `dfy` 命令行入口：genkey / demo-agent / demo-client / selftest。
//!
//! 所有子命令都是 **demo / 诊断**用途，不是产品形态的客户端或 Agent。

#![forbid(unsafe_code)]

mod adapters;
mod demo;
mod genkey;
mod real;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use dfy_transport::{human_bytes, parse_fingerprint_hex};

/// 大肥鱼远程 (DFYP/1) 命令行工具
#[derive(Debug, Parser)]
#[command(
    name = "dfy",
    version,
    about = "大肥鱼远程 (DFYP/1) 命令行工具：密钥生成 / QUIC demo / 端到端自测",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// 生成 Ed25519 设备密钥，打印 device key id（BLAKE3 前 16 字节 base32）
    Genkey {
        /// 输出目录（默认 .tmp/devkeys）
        #[arg(long, value_name = "DIR", default_value = ".tmp/devkeys")]
        out: PathBuf,
    },
    /// 起 demo Agent：QUIC 监听、握手、推视频/输入 DATAGRAM、心跳
    DemoAgent {
        /// 监听地址（IP）
        #[arg(long, default_value = "0.0.0.0")]
        host: String,
        /// 监听 UDP 端口
        #[arg(long, default_value_t = 21116)]
        port: u16,
        /// 演示口令（MVP: HMAC-SHA256(SHA256(pw), nonce)）
        #[arg(long, default_value = "test")]
        password: String,
    },
    /// 起 demo Client：连接、握手、收 N 秒数据、打印统计、Bye
    DemoClient {
        /// Agent 地址，例如 127.0.0.1:21116
        #[arg(long, default_value = "127.0.0.1:21116")]
        addr: String,
        /// 演示口令
        #[arg(long, default_value = "test")]
        password: String,
        /// 严格校验服务端证书 SPKI 指纹（64 位 hex，可含冒号/大写）
        #[arg(long, value_name = "HEX")]
        expect_fingerprint: Option<String>,
        /// 接收时长（秒）
        #[arg(long, default_value_t = 3)]
        seconds: u64,
    },
    /// 端到端自测：进程内 agent + client，跑 1 秒，断言握手与 ≥10 个视频 DATAGRAM
    /// 端到端自测：进程内 agent + client，跑 1 秒，断言握手与 ≥10 个视频 DATAGRAM
    Selftest,
    /// 真实被控端：采集屏幕 + H.264 编码 → QUIC 推流（Ctrl+C 退出）
    Agent {
        /// 监听地址（IP）
        #[arg(long, default_value = "0.0.0.0")]
        host: String,
        /// 监听 UDP 端口
        #[arg(long, default_value_t = 21116)]
        port: u16,
        /// 连接口令
        #[arg(long, default_value = "test")]
        password: String,
        /// 采集哪个显示器（DXGI 枚举索引）
        #[arg(long, default_value_t = 0)]
        monitor: u32,
        /// 目标帧率
        #[arg(long, default_value_t = 60)]
        fps: u32,
        /// 目标码率（kbps）
        #[arg(long, default_value_t = 8000)]
        bitrate: u32,
    },
    /// 真实主控端：连接 Agent、解码 H.264、统计并可保存首帧
    View {
        /// Agent 地址，例如 127.0.0.1:21116
        #[arg(long, default_value = "127.0.0.1:21116")]
        addr: String,
        /// 连接口令
        #[arg(long, default_value = "test")]
        password: String,
        /// 严格校验 Agent 证书 SPKI 指纹（64 位 hex）
        #[arg(long, value_name = "HEX")]
        expect_fingerprint: Option<String>,
        /// 观察时长（秒）
        #[arg(long, default_value_t = 10)]
        seconds: u64,
        /// 把第一帧解码结果存成裸 BGRA 文件
        #[arg(long, value_name = "FILE")]
        save_first: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    match Cli::parse().command {
        Command::Genkey { out } => genkey::run(&out),
        Command::DemoAgent {
            host,
            port,
            password,
        } => run_demo_agent(&host, port, password).await,
        Command::DemoClient {
            addr,
            password,
            expect_fingerprint,
            seconds,
        } => run_demo_client(&addr, password, expect_fingerprint, seconds).await,
        Command::Selftest => demo::selftest().await,
        Command::Agent {
            host,
            port,
            password,
            monitor,
            fps,
            bitrate,
        } => {
            let addr: SocketAddr = format!("{host}:{port}")
                .parse()
                .with_context(|| format!("无法解析监听地址 {host}:{port}"))?;
            real::run_agent(real::AgentOptions {
                addr,
                password,
                monitor,
                fps,
                bitrate_kbps: bitrate,
                max_frames: None,
            })
            .await
        }
        Command::View {
            addr,
            password,
            expect_fingerprint,
            seconds,
            save_first,
        } => {
            let addr: SocketAddr = addr
                .parse()
                .with_context(|| format!("无法解析 Agent 地址 {addr}（示例：127.0.0.1:21116）"))?;
            let expect = match expect_fingerprint {
                Some(text) => Some(real::parse_fingerprint_arg(&text)?),
                None => None,
            };
            real::run_view(real::ViewOptions {
                addr,
                password,
                expect_fingerprint: expect,
                seconds,
                save_first_frame: save_first,
            })
            .await
            .map(|_| ())
        }
    }
}

/// 初始化 tracing：默认 `info`，可用 `RUST_LOG` 覆盖（例如 `RUST_LOG=dfy_transport=debug`）。
fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .try_init();
}

async fn run_demo_agent(host: &str, port: u16, password: String) -> Result<()> {
    let addr: SocketAddr = format!("{host}:{port}")
        .parse()
        .with_context(|| format!("无法解析监听地址 {host}:{port}"))?;
    let (endpoint, identity) = dfy_transport::bind_agent(addr)
        .with_context(|| format!("在 {addr} 上启动 QUIC Agent 失败（UDP 端口可能被占用）"))?;
    let local = endpoint.local_addr().context("读取本地监听地址失败")?;
    println!("[demo-agent] 监听 {local}（UDP/QUIC，ALPN = dfyp/1）");
    println!(
        "[demo-agent] 证书 SPKI-SHA256 指纹 = {}",
        identity.fingerprint_hex()
    );
    println!("[demo-agent] 短指纹 = {}", identity.fingerprint_short());
    println!(
        "[demo-agent] 口令 = {password}（MVP: HMAC-SHA256(key=SHA256(口令), nonce)；生产应换 Argon2id）"
    );
    println!("[demo-agent] 握手成功后：60 fps 视频 DATAGRAM（1200B 伪造帧，每 60 帧一个 keyframe）+ 20/s 鼠标移动 + 5s 心跳");
    println!("[demo-agent] 按 Ctrl+C 停止");

    tokio::select! {
        served = demo::serve(endpoint.clone(), password) => {
            served.context("Agent 服务循环出错")?;
        }
        _ = tokio::signal::ctrl_c() => {
            println!("\n[demo-agent] 收到 Ctrl+C，正在关闭");
        }
    }
    endpoint.close(0u32.into(), b"agent shutdown");
    println!("[demo-agent] 已退出");
    Ok(())
}

async fn run_demo_client(
    addr_text: &str,
    password: String,
    expect_fingerprint: Option<String>,
    seconds: u64,
) -> Result<()> {
    let addr: SocketAddr = addr_text
        .parse()
        .with_context(|| format!("无法解析 Agent 地址 {addr_text}（示例：127.0.0.1:21116）"))?;
    let expect = match expect_fingerprint {
        Some(text) => Some(
            parse_fingerprint_hex(&text)
                .context("解析 --expect-fingerprint 失败（需要 64 位 hex）")?,
        ),
        None => None,
    };
    println!("[demo-client] 连接 {addr} ...");
    let stats = demo::run_client(
        addr,
        &password,
        expect,
        Duration::from_secs(seconds.max(1)),
        "dfy-demo-client",
    )
    .await?;

    println!("[demo-client] ---- 统计（{seconds}s）----");
    println!(
        "[demo-client] 视频帧 = {}，DATAGRAM 包 = {}（{}），key 包 = {}，LAST 包 = {}",
        stats.video_frames,
        stats.video_packets,
        human_bytes(stats.video_bytes),
        stats.key_packets,
        stats.last_packets
    );
    println!(
        "[demo-client] input-move DATAGRAM = {}，控制帧 = {}",
        stats.input_packets, stats.control_in
    );
    match stats.rtt_ms {
        Some(rtt) => println!(
            "[demo-client] 心跳 RTT = {rtt:.3} ms（ping 发送 {} 次，pong 收到 {} 次）",
            stats.pings_sent, stats.pongs
        ),
        None => println!("[demo-client] 心跳 RTT = 未测到（会话太短或 Pong 未回）"),
    }
    if let Some((w, h, fps, kbps)) = stats.video_config {
        println!("[demo-client] VideoConfig = {w}x{h} @ {fps} fps, {kbps} kbps");
    }
    if stats.monitors > 0 {
        println!("[demo-client] MonitorList = {} 个显示器", stats.monitors);
    }
    if stats.video_frames == 0 {
        println!("[demo-client] 警告：本次未收到任何完整视频帧（可能握手后立即退出）");
    }
    Ok(())
}
