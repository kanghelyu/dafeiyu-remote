//! `dfy` 命令行入口：genkey / demo-agent / demo-client / selftest / agent / view。
//!
//! 所有子命令都是 **demo / 诊断**用途，不是产品形态的客户端或 Agent。

// 除 Windows 输入采集模块外不允许 unsafe；`input_capture` 用 `#![allow(unsafe_code)]`
// 局部放开（Win32 钩子 API 必须 unsafe）。这里用 `deny` 而非 `forbid`，否则无法覆盖。
#![deny(unsafe_code)]
// macOS 等平台的采集/注入后端尚未实现（M3），相应辅助类型暂不参与调用路径。
#![cfg_attr(
    not(any(windows, target_os = "linux")),
    allow(dead_code, unused_variables)
)]

mod adapters;
mod clipboard_bridge;
mod credentials;
mod demo;
mod file_bridge;
mod genkey;
mod input_bridge;
mod input_capture;
mod p2p_bridge;
mod real;
mod relay_bridge;
mod relay_profiles_cmd;
mod virtual_display;
mod wol;

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use dfy_transport::{human_bytes, parse_fingerprint_hex};

use crate::credentials::{PasswordArgs, SessionPassword};

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

#[derive(Debug, Clone, clap::Args)]
struct VirtualDisplayMode {
    /// 虚拟显示宽度（像素，必须为偶数）
    #[arg(long, default_value_t = 1920)]
    virtual_width: u32,
    /// 虚拟显示高度（像素，必须为偶数）
    #[arg(long, default_value_t = 1080)]
    virtual_height: u32,
    /// 虚拟显示刷新率；Linux Xvfb 仅保存该元数据
    #[arg(long, default_value_t = 60)]
    virtual_refresh: u32,
    /// 虚拟显示 DPI；Linux Xvfb 仅保存该元数据
    #[arg(long, default_value_t = 96)]
    virtual_dpi: u32,
    /// 虚拟显示名称（可打印 ASCII）
    #[arg(long, default_value = "dfy-virtual")]
    virtual_name: String,
}

impl VirtualDisplayMode {
    fn spec(&self) -> dfy_vdisplay::VirtualDisplaySpec {
        dfy_vdisplay::VirtualDisplaySpec::new(
            self.virtual_width,
            self.virtual_height,
            self.virtual_refresh,
            self.virtual_dpi,
            self.virtual_name.clone(),
        )
    }
}

#[derive(Debug, Subcommand)]
enum VdisplayAction {
    /// 列出当前用户的虚拟显示
    List,
    /// 修改已有虚拟显示几何；Linux 不支持运行时改刷新率/DPI
    Update {
        /// 虚拟显示 ID
        #[arg(long)]
        id: u32,
        #[command(flatten)]
        mode: VirtualDisplayMode,
    },
    /// 删除一个虚拟显示
    Destroy {
        /// 虚拟显示 ID
        #[arg(long)]
        id: u32,
    },
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
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// 监听 UDP 端口
        #[arg(long, default_value_t = 21116)]
        port: u16,
        /// 演示口令（MVP: HMAC-SHA256(SHA256(pw), nonce)）
        #[command(flatten)]
        password: PasswordArgs,
    },
    /// 起 demo Client：连接、握手、收 N 秒数据、打印统计、Bye
    DemoClient {
        /// Agent 地址，例如 127.0.0.1:21116
        #[arg(long, default_value = "127.0.0.1:21116")]
        addr: String,
        /// 演示口令
        #[command(flatten)]
        password: PasswordArgs,
        /// 严格校验服务端证书 SPKI 指纹（64 位 hex，可含冒号/大写）
        #[arg(long, value_name = "HEX")]
        expect_fingerprint: Option<String>,
        /// 接收时长（秒）
        #[arg(long, default_value_t = 3)]
        seconds: u64,
    },
    /// 端到端自测：进程内 agent + client，跑 1 秒，断言握手与 ≥10 个视频 DATAGRAM
    Selftest,
    /// 发送标准 Wake-on-LAN 魔法包（仅适用于已授权局域网）
    Wol {
        /// 目标网卡 MAC 地址
        #[arg(long, value_name = "MAC")]
        mac: String,
        /// IPv4 广播地址
        #[arg(long, value_name = "IP", default_value = "255.255.255.255")]
        broadcast: IpAddr,
        /// UDP 目标端口（通常为 7 或 9）
        #[arg(long, default_value_t = 9)]
        port: u16,
        /// 重复发送次数
        #[arg(long, default_value_t = 3)]
        repeats: u8,
    },
    /// 管理虚拟显示：列表、几何更新、删除
    Vdisplay {
        #[command(subcommand)]
        action: VdisplayAction,
    },
    /// 管理用户自建中继池
    RelayProfile {
        /// 中继池配置文件
        #[arg(
            long,
            global = true,
            value_name = "FILE",
            default_value = "dfy-relay-profiles.toml"
        )]
        config: PathBuf,
        #[command(subcommand)]
        action: relay_profiles_cmd::RelayProfileAction,
    },
    /// 真实被控端：采集屏幕 + H.264 编码 → QUIC 推流（Ctrl+C 退出）
    Agent {
        /// 监听地址（IP）
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// 监听 UDP 端口
        #[arg(long, default_value_t = 21116)]
        port: u16,
        /// 连接口令
        #[command(flatten)]
        password: PasswordArgs,
        /// 采集哪个显示器（DXGI 枚举索引）
        #[arg(long, default_value_t = 0)]
        monitor: u32,
        /// 创建独立虚拟显示数量（0=使用物理桌面；Linux 为隔离 Xvfb）
        #[arg(long, default_value_t = 0)]
        virtual_displays: u32,
        /// 选择要采集的第几个虚拟显示（从 0 开始）
        #[arg(long, default_value_t = 0)]
        virtual_monitor: u32,
        #[command(flatten)]
        virtual_display: VirtualDisplayMode,
        /// 目标帧率
        #[arg(long, default_value_t = 60)]
        fps: u32,
        /// 目标码率（kbps）
        #[arg(long, default_value_t = 8000)]
        bitrate: u32,
        /// 允许把控制端发来的鼠标/键盘注入本机（默认关闭）
        #[arg(long, default_value_t = false)]
        allow_input: bool,
        /// 允许与本机剪贴板双向同步（默认关闭）
        #[arg(long, default_value_t = false)]
        allow_clipboard: bool,
        /// 采集本机音频并发送给主控（默认关闭；需要音频输入设备）
        #[arg(long, default_value_t = false)]
        audio_out: bool,
        /// 诊断：用合成正弦波代替麦克风（需配合 --audio-out）
        #[arg(long, default_value_t = false)]
        audio_test_tone: bool,
        /// 经中继接入（UDP 地址，例如 relay.example.com:21117）
        #[arg(long, value_name = "ADDR")]
        relay: Option<String>,
        /// 会合 tag（32 位 hex；使用 --relay 时必填）
        #[arg(long, value_name = "HEX")]
        relay_tag: Option<String>,
        /// 中继 relay_key（hex:…/base64:…/裸 hex）
        #[arg(long, value_name = "KEY")]
        relay_key: Option<String>,
        /// 使用中继池中的档案（ID 或 @current），与 --relay 互斥；仍需 --relay-tag
        #[arg(long, value_name = "ID", conflicts_with = "relay")]
        relay_profile: Option<String>,
        /// 中继池配置文件
        #[arg(long, value_name = "FILE", default_value = "dfy-relay-profiles.toml")]
        relay_profiles_config: PathBuf,
        /// 质量驱动双路径：Agent 同时监听直连与中继；View 劣化时迁移到中继
        #[arg(long, default_value_t = false)]
        auto_path: bool,
        /// 诊断用：首个采样周期强制迁移到 direct|relay 一次
        #[arg(long, value_name = "PATH")]
        force_switch_once: Option<String>,
        /// --auto-path 起始路径：direct|relay（默认 direct）
        #[arg(long, value_name = "PATH")]
        auto_initial_path: Option<String>,
        /// 启用 P2P 直连打洞（配合 --peer 或按提示粘贴对端候选）
        #[arg(long, default_value_t = false)]
        p2p: bool,
        /// STUN 服务器（逗号分隔，可多次；例如 stun.l.google.com:19302）
        #[arg(long, value_name = "ADDR")]
        stun: Option<String>,
        /// 对端候选（逗号/空格分隔；省略时从 stdin 读取）
        #[arg(long, value_name = "ADDRS")]
        peer: Option<String>,
        /// P2P 本端 UDP 端口（0 = 内核分配；固定端口便于人工交换候选）
        #[arg(long, default_value_t = 0)]
        p2p_port: u16,
        /// 打洞窗口（毫秒）
        #[arg(long, default_value_t = 3000)]
        punch_timeout_ms: u64,
        /// 视频编码格式：h264（Windows 默认，MF 硬编优先）/ mjpeg（跨平台兜底，非 Windows 默认）
        #[arg(long, value_name = "CODEC")]
        codec: Option<String>,
        /// 接收控制端发来的文件到该目录（默认关闭）
        #[arg(long, value_name = "DIR")]
        recv_dir: Option<PathBuf>,
    },
    /// 真实主控端：连接 Agent、解码 H.264、统计并可保存首帧
    View {
        /// Agent 地址，例如 127.0.0.1:21116
        #[arg(long, default_value = "127.0.0.1:21116")]
        addr: String,
        /// 连接口令
        #[command(flatten)]
        password: PasswordArgs,
        /// 严格校验 Agent 证书 SPKI 指纹（64 位 hex）
        #[arg(long, value_name = "HEX")]
        expect_fingerprint: Option<String>,
        /// 观察时长（秒）
        #[arg(long, default_value_t = 10)]
        seconds: u64,
        /// 把第一帧解码结果存成裸 BGRA 文件
        #[arg(long, value_name = "FILE")]
        save_first: Option<PathBuf>,
        /// 启用控制输入采集（Windows 低级钩子 / Linux X11 XInput2；Ctrl+Alt+Shift+Q 停止）
        #[arg(long, default_value_t = false)]
        control: bool,
        /// 打开图形窗口显示远端画面（需要图形环境）
        #[arg(long, default_value_t = false)]
        ui: bool,
        /// UI 窗口启动即全屏（配合 --ui）
        #[arg(long, default_value_t = false)]
        fullscreen: bool,
        /// 启用剪贴板同步（与本机剪贴板双向同步）
        #[arg(long, default_value_t = false)]
        clipboard: bool,
        /// 接收 Agent 音频并用默认输出设备播放
        #[arg(long, default_value_t = false)]
        audio_in: bool,
        /// 播放音量 0..=100（配合 --audio-in）
        #[arg(long, default_value_t = 100)]
        volume: u8,
        /// 静音（配合 --audio-in；仍接收/解码音频）
        #[arg(long, default_value_t = false)]
        mute: bool,
        /// 经中继接入（UDP 地址，例如 relay.example.com:21117）
        #[arg(long, value_name = "ADDR")]
        relay: Option<String>,
        /// 会合 tag（32 位 hex；使用 --relay 时必填）
        #[arg(long, value_name = "HEX")]
        relay_tag: Option<String>,
        /// 中继 relay_key（hex:…/base64:…/裸 hex）
        #[arg(long, value_name = "KEY")]
        relay_key: Option<String>,
        /// 使用中继池中的档案（ID 或 @current），与 --relay 互斥；仍需 --relay-tag
        #[arg(long, value_name = "ID", conflicts_with = "relay")]
        relay_profile: Option<String>,
        /// 中继池配置文件
        #[arg(long, value_name = "FILE", default_value = "dfy-relay-profiles.toml")]
        relay_profiles_config: PathBuf,
        /// 质量驱动双路径：Agent 同时监听直连与中继；View 劣化时迁移到中继
        #[arg(long, default_value_t = false)]
        auto_path: bool,
        /// 诊断用：首个采样周期强制迁移到 direct|relay 一次
        #[arg(long, value_name = "PATH")]
        force_switch_once: Option<String>,
        /// --auto-path 起始路径：direct|relay（默认 direct）
        #[arg(long, value_name = "PATH")]
        auto_initial_path: Option<String>,
        /// 启用 P2P 直连打洞（配合 --peer 或按提示粘贴对端候选）
        #[arg(long, default_value_t = false)]
        p2p: bool,
        /// STUN 服务器（逗号分隔；例如 stun.l.google.com:19302）
        #[arg(long, value_name = "ADDR")]
        stun: Option<String>,
        /// 对端候选（逗号/空格分隔；省略时从 stdin 读取）
        #[arg(long, value_name = "ADDRS")]
        peer: Option<String>,
        /// P2P 本端 UDP 端口（0 = 内核分配）
        #[arg(long, default_value_t = 0)]
        p2p_port: u16,
        /// 打洞窗口（毫秒）
        #[arg(long, default_value_t = 3000)]
        punch_timeout_ms: u64,
        /// 视频编码格式：h264 / mjpeg（必须与 Agent 一致；默认 Windows=h264，其它=mjpeg）
        #[arg(long, value_name = "CODEC")]
        codec: Option<String>,
        /// 请求视频宽度上限（像素，必须为偶数；四个画质参数需一起提供）
        #[arg(long, value_name = "PX")]
        quality_width: Option<u32>,
        /// 请求视频高度上限（像素，必须为偶数；四个画质参数需一起提供）
        #[arg(long, value_name = "PX")]
        quality_height: Option<u32>,
        /// 请求视频帧率上限（1..=120；四个画质参数需一起提供）
        #[arg(long, value_name = "FPS")]
        quality_fps: Option<u32>,
        /// 请求视频码率上限（100..=100000 kbps；四个画质参数需一起提供）
        #[arg(long, value_name = "KBPS")]
        quality_bitrate: Option<u32>,
        /// 恢复 Agent 启动时的画质上限（与四个画质参数互斥）
        #[arg(
            long,
            conflicts_with_all = [
                "quality_width",
                "quality_height",
                "quality_fps",
                "quality_bitrate"
            ]
        )]
        reset_quality: bool,
        /// 发送本地文件/目录到 Agent（可多次；需要 Agent 开启 --recv-dir）
        #[arg(long, value_name = "PATH")]
        send_file: Vec<PathBuf>,
        /// 诊断用：候选路径主动探测包大小（64..=1200 字节，默认 1200）
        #[arg(long, hide = true, default_value_t = 1200)]
        probe_bytes: usize,
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
        } => run_demo_agent(&host, port, password.read()?).await,
        Command::DemoClient {
            addr,
            password,
            expect_fingerprint,
            seconds,
        } => run_demo_client(&addr, password.read()?, expect_fingerprint, seconds).await,
        Command::Selftest => Box::pin(demo::selftest()).await,
        Command::Wol {
            mac,
            broadcast,
            port,
            repeats,
        } => {
            let mac = wol::MacAddr::parse(&mac)?;
            wol::send(mac, broadcast, port, repeats)?;
            println!(
                "已向 {broadcast}:{port} 发送 {} 个 Wake-on-LAN 包（{mac}）；对方是否启动需在目标设备确认",
                repeats
            );
            Ok(())
        }
        Command::Vdisplay { action } => match action {
            VdisplayAction::List => {
                let displays = tokio::task::spawn_blocking(virtual_display::list).await??;
                if displays.is_empty() {
                    println!("没有虚拟显示");
                }
                for display in displays {
                    println!(
                        "id={} attached={} {}x{} {}Hz {}DPI {}",
                        display.id,
                        display.attached,
                        display.spec.width,
                        display.spec.height,
                        display.spec.refresh_hz,
                        display.spec.dpi,
                        display.spec.name_hint
                    );
                }
                Ok(())
            }
            VdisplayAction::Update { id, mode } => {
                let spec = mode.spec();
                tokio::task::spawn_blocking(move || virtual_display::update(id, &spec)).await??;
                println!("已更新虚拟显示 {id}");
                Ok(())
            }
            VdisplayAction::Destroy { id } => {
                tokio::task::spawn_blocking(move || virtual_display::destroy(id)).await??;
                println!("已删除虚拟显示 {id}");
                Ok(())
            }
        },
        Command::RelayProfile { config, action } => relay_profiles_cmd::run(&config, action).await,
        Command::Agent {
            host,
            port,
            password,
            monitor,
            virtual_displays,
            virtual_monitor,
            virtual_display,
            fps,
            bitrate,
            allow_input,
            allow_clipboard,
            relay,
            relay_tag,
            relay_key,
            relay_profile,
            relay_profiles_config,
            auto_path,
            force_switch_once,
            auto_initial_path,
            p2p,
            stun,
            peer,
            p2p_port,
            punch_timeout_ms,
            codec,
            recv_dir,
            audio_out,
            audio_test_tone,
        } => {
            let password = password.read()?;
            let relay_options = build_relay(
                relay,
                relay_tag,
                relay_key,
                relay_profile,
                &relay_profiles_config,
            )
            .await?;
            let p2p_options = build_p2p(p2p, stun, peer, p2p_port, punch_timeout_ms)?;
            let addr: SocketAddr = format!("{host}:{port}")
                .parse()
                .with_context(|| format!("无法解析监听地址 {host}:{port}"))?;
            Box::pin(real::run_agent(real::AgentOptions {
                addr,
                password,
                monitor,
                virtual_displays,
                virtual_monitor,
                virtual_spec: virtual_display.spec(),
                fps,
                bitrate_kbps: bitrate,
                max_frames: None,
                allow_input,
                allow_clipboard,
                audio_out,
                audio_test_tone,
                relay: relay_options,
                auto_path,
                force_switch_once: force_switch_once
                    .as_deref()
                    .map(parse_path_kind)
                    .transpose()?,
                auto_initial_path: auto_initial_path
                    .as_deref()
                    .map(parse_path_kind)
                    .transpose()?,
                p2p: p2p_options,
                codec: parse_codec(codec)?,
                recv_dir,
            }))
            .await
        }
        Command::View {
            addr,
            password,
            expect_fingerprint,
            seconds,
            save_first,
            control,
            ui,
            fullscreen,
            clipboard,
            relay,
            relay_tag,
            relay_key,
            relay_profile,
            relay_profiles_config,
            auto_path,
            force_switch_once,
            auto_initial_path,
            p2p,
            stun,
            peer,
            p2p_port,
            punch_timeout_ms,
            codec,
            quality_width,
            quality_height,
            quality_fps,
            quality_bitrate,
            reset_quality,
            send_file,
            probe_bytes,
            audio_in,
            volume,
            mute,
        } => {
            let password = password.read()?;
            let relay_options = build_relay(
                relay,
                relay_tag,
                relay_key,
                relay_profile,
                &relay_profiles_config,
            )
            .await?;
            let p2p_options = build_p2p(p2p, stun, peer, p2p_port, punch_timeout_ms)?;
            let addr: SocketAddr = addr
                .parse()
                .with_context(|| format!("无法解析 Agent 地址 {addr}（示例：127.0.0.1:21116）"))?;
            let expect = match expect_fingerprint {
                Some(text) => Some(real::parse_fingerprint_arg(&text)?),
                None => None,
            };
            Box::pin(real::run_view(real::ViewOptions {
                addr,
                password,
                expect_fingerprint: expect,
                seconds,
                save_first_frame: save_first,
                control,
                ui,
                fullscreen,
                clipboard,
                audio_in,
                audio_volume: volume,
                audio_mute: mute,
                relay: relay_options,
                auto_path,
                force_switch_once: force_switch_once
                    .as_deref()
                    .map(parse_path_kind)
                    .transpose()?,
                auto_initial_path: auto_initial_path
                    .as_deref()
                    .map(parse_path_kind)
                    .transpose()?,
                p2p: p2p_options,
                codec: parse_codec(codec)?,
                quality_width,
                quality_height,
                quality_fps,
                quality_bitrate,
                reset_quality,
                send_files: send_file,
                probe_bytes: probe_bytes.clamp(64, 1200),
            }))
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

/// 解析诊断用路径参数。
fn parse_path_kind(text: &str) -> Result<dfy_path::PathKind> {
    match text.trim().to_ascii_lowercase().as_str() {
        "direct" | "p2p" => Ok(dfy_path::PathKind::Direct),
        "relay" => Ok(dfy_path::PathKind::Relay),
        other => anyhow::bail!("路径非法：{other}（可选 direct|relay）"),
    }
}

/// 解析视频编码格式；未指定时按平台给默认值（Windows 走 MF H.264，其它平台走 MJPEG 兜底）。
fn parse_codec(text: Option<String>) -> Result<dfy_codec::Codec> {
    match text.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(value) => match value.to_ascii_lowercase().as_str() {
            "h264" | "avc" => Ok(dfy_codec::Codec::H264),
            "mjpeg" | "jpeg" => Ok(dfy_codec::Codec::Mjpeg),
            "h265" | "hevc" => anyhow::bail!("--codec h265 暂不支持（v1 只有 h264 / mjpeg）"),
            other => anyhow::bail!("未知 --codec {other}（可选：h264 / mjpeg）"),
        },
        None => Ok(if cfg!(windows) {
            dfy_codec::Codec::H264
        } else {
            dfy_codec::Codec::Mjpeg
        }),
    }
}

/// 解析中继参数：直接地址或中继池档案，二选一；tag 必须显式共享。
async fn build_relay(
    relay: Option<String>,
    relay_tag: Option<String>,
    relay_key: Option<String>,
    relay_profile: Option<String>,
    relay_profiles_config: &PathBuf,
) -> Result<Option<relay_bridge::RelayOptions>> {
    if relay_profile.is_some() {
        anyhow::ensure!(relay.is_none(), "--relay 与 --relay-profile 不能同时使用");
        anyhow::ensure!(
            relay_key.is_none(),
            "--relay-key 不能与 --relay-profile 同时使用（密钥来自档案）"
        );
    }
    if relay.is_none() && relay_profile.is_none() {
        anyhow::ensure!(
            relay_tag.is_none() && relay_key.is_none(),
            "--relay-tag/--relay-key 需要与 --relay 或 --relay-profile 一起使用"
        );
        return Ok(None);
    }
    let tag =
        relay_tag.with_context(|| "使用中继需要 --relay-tag（两端显式共享的 16 字节 tag）")?;

    let (addr_text, key_text, display) = if let Some(profile_id) = relay_profile {
        let store = dfy_relay_profiles::RelayProfileStore::load(relay_profiles_config)?;
        let profile = if profile_id == "@current" {
            store
                .selected()
                .with_context(|| "中继池为空，无法使用 @current")?
        } else {
            store
                .get(&profile_id)
                .with_context(|| format!("找不到中继档案：{profile_id}"))?
        };
        let display = format!("{} ({})", profile.name, profile.endpoint());
        (
            profile.endpoint(),
            profile.key_config().to_string(),
            display,
        )
    } else {
        let addr_text = relay.expect("checked above");
        let key_text = relay_key
            .as_deref()
            .with_context(|| "--relay 需要同时提供 --relay-key（中继服务器的 relay_key）")?
            .to_string();
        (addr_text.clone(), key_text, addr_text)
    };

    let mut addrs = tokio::net::lookup_host(addr_text.as_str())
        .await
        .with_context(|| format!("无法解析中继地址 {addr_text}"))?;
    let addr = addrs
        .next()
        .with_context(|| format!("中继地址 {addr_text} 未解析出可用端点"))?;
    let options = relay_bridge::RelayOptions::new(addr, &tag, &key_text)?;
    println!(
        "[relay] 目标 {}（{}），tag {}",
        display,
        options.addr,
        relay_bridge::tag_hex(&options.tag)
    );
    Ok(Some(options))
}

/// 组装 P2P 打洞参数。`--tag/--key` 这类只在 relay 下有效的参数不在此处理。
fn build_p2p(
    enabled: bool,
    stun: Option<String>,
    peer: Option<String>,
    local_port: u16,
    punch_timeout_ms: u64,
) -> Result<Option<real::P2POptions>> {
    if !enabled {
        if stun.is_some() || peer.is_some() {
            anyhow::bail!("--stun/--peer 需要与 --p2p 一起使用");
        }
        return Ok(None);
    }
    let stun_servers = match stun {
        Some(text) => crate::p2p_bridge::parse_stun_list(&text)?,
        None => Vec::new(),
    };
    let peers = match peer {
        Some(text) => crate::p2p_bridge::parse_candidates(&text)?,
        None => Vec::new(),
    };
    Ok(Some(real::P2POptions {
        stun: stun_servers,
        peer: peers,
        local_port,
        punch_timeout: std::time::Duration::from_millis(punch_timeout_ms.max(200)),
    }))
}

async fn run_demo_agent(host: &str, port: u16, password: SessionPassword) -> Result<()> {
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
    password: SessionPassword,
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
        password.expose(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_defaults_are_local_and_capabilities_are_opt_in() {
        let cli = Cli::try_parse_from(["dfy", "agent"]).expect("parse agent defaults");
        let Command::Agent {
            host,
            allow_input,
            allow_clipboard,
            ..
        } = cli.command
        else {
            panic!("expected agent command");
        };
        assert_eq!(host, "127.0.0.1");
        assert!(!allow_input);
        assert!(!allow_clipboard);
    }

    #[tokio::test]
    async fn relay_requires_an_explicit_tag() {
        let path = PathBuf::from("unused-relay-profiles.toml");
        let result = build_relay(
            Some("127.0.0.1:21117".into()),
            None,
            Some("00".repeat(32)),
            None,
            &path,
        )
        .await;
        let error = result.expect_err("missing tag must fail");
        assert!(error.to_string().contains("--relay-tag"));
    }

    #[tokio::test]
    async fn relay_profile_uses_selected_server() {
        use dfy_relay::config::RelayKey;
        use dfy_relay_profiles::{RelayProfile, RelayProfileStore, TlsMode};

        let path = std::env::temp_dir().join(format!(
            "dfy-relay-cli-profile-test-{}.toml",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let key = RelayKey::random().to_config_string();
        let mut store = RelayProfileStore::new(&path);
        store
            .add(
                RelayProfile::new(
                    "home",
                    "Home Relay",
                    "127.0.0.1",
                    21117,
                    key,
                    None,
                    1,
                    true,
                    TlsMode::Auto,
                )
                .expect("profile"),
            )
            .expect("add");
        store.save().expect("save");

        let result = build_relay(
            None,
            Some("00".repeat(16)),
            None,
            Some("@current".to_string()),
            &path,
        )
        .await
        .expect("build relay")
        .expect("profile selected");
        assert_eq!(result.addr.to_string(), "127.0.0.1:21117");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn view_quality_options_are_optional_and_reset_is_separate() {
        let cli = Cli::try_parse_from(["dfy", "view"]).expect("parse view defaults");
        let Command::View {
            quality_width,
            quality_height,
            quality_fps,
            quality_bitrate,
            reset_quality,
            ..
        } = cli.command
        else {
            panic!("expected view command");
        };
        assert_eq!(quality_width, None);
        assert_eq!(quality_height, None);
        assert_eq!(quality_fps, None);
        assert_eq!(quality_bitrate, None);
        assert!(!reset_quality);

        let cli = Cli::try_parse_from([
            "dfy",
            "view",
            "--quality-width",
            "1280",
            "--quality-height",
            "720",
            "--quality-fps",
            "30",
            "--quality-bitrate",
            "4000",
        ])
        .expect("parse view quality request");
        let Command::View {
            quality_width,
            quality_height,
            quality_fps,
            quality_bitrate,
            reset_quality,
            ..
        } = cli.command
        else {
            panic!("expected view command");
        };
        assert_eq!(quality_width, Some(1280));
        assert_eq!(quality_height, Some(720));
        assert_eq!(quality_fps, Some(30));
        assert_eq!(quality_bitrate, Some(4000));
        assert!(!reset_quality);

        let cli = Cli::try_parse_from(["dfy", "view", "--reset-quality"])
            .expect("parse view quality reset");
        let Command::View {
            quality_width,
            quality_height,
            quality_fps,
            quality_bitrate,
            reset_quality,
            ..
        } = cli.command
        else {
            panic!("expected view command");
        };
        assert_eq!(quality_width, None);
        assert_eq!(quality_height, None);
        assert_eq!(quality_fps, None);
        assert_eq!(quality_bitrate, None);
        assert!(reset_quality);
    }

    #[test]
    fn view_reset_quality_conflicts_with_every_request_field() {
        for option in [
            "--quality-width",
            "--quality-height",
            "--quality-fps",
            "--quality-bitrate",
        ] {
            let error = Cli::try_parse_from(["dfy", "view", "--reset-quality", option, "1280"])
                .expect_err("reset and quality request must conflict");
            let message = error.to_string();
            assert!(message.contains("--reset-quality"), "{message}");
            assert!(message.contains(option), "{message}");
        }
    }
}
