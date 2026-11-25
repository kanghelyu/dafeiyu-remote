// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 真实链路：屏幕采集 + Media Foundation H.264 编码 → QUIC → 解码。
//!
//! 这是 MVP 的「能看见画面」验证路径：`dfy agent`（被控端）与 `dfy view`（主控端）。
//! 目前只传视频 DATAGRAM + 心跳；输入注入/剪贴板/文件会在此之上继续接。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use dfy_proto::envelope;
use dfy_relay::Role;
use dfy_session::{
    serve_agent, start_capture_loop, AgentConfig, CaptureLoopConfig, Decoder as _, FrameAssembler,
    RawFrame,
};
use dfy_transport::{
    agent_endpoint_on_socket, bind_agent, client_handshake, client_tls, connect, connect_on_socket,
    parse_fingerprint_hex, send_input_move, ClientSession, ServerAuth, TlsIdentity,
};

use crate::relay_bridge::RelayOptions;
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::adapters::{MftDecoderAdapter, MftEncoderAdapter};
use crate::clipboard_bridge::{AgentHandler, ViewClipboard};
use crate::input_capture::{CtrlMsg, InputCapture};
pub use crate::p2p_bridge::P2POptions;

/// 被控端（Agent）参数。
#[derive(Debug, Clone)]
pub struct AgentOptions {
    /// 监听地址（UDP/QUIC）。
    pub addr: SocketAddr,
    /// 连接口令（MVP: HMAC-SHA256(SHA256(pw), nonce)）。
    pub password: String,
    /// 采集哪个显示器（索引来自 DXGI 枚举顺序）。
    pub monitor: u32,
    /// 目标帧率。
    pub fps: u32,
    /// 目标码率（kbps）。
    pub bitrate_kbps: u32,
    /// 送满多少帧后结束一个会话（`None` = 一直跑）。
    pub max_frames: Option<u64>,
    /// 是否把控制端发来的输入注入本机（`--no-input` 关闭）。
    pub allow_input: bool,
    /// 是否与客户端双向同步剪贴板（`--no-clipboard` 关闭）。
    pub allow_clipboard: bool,
    /// 经中继接入（`None` = 直连/公网打洞）。
    pub relay: Option<RelayOptions>,
    /// P2P 直连打洞（`None` = 不做打洞，直接 bind/connect）。
    pub p2p: Option<P2POptions>,
    /// 视频编码格式（Windows 默认 H.264；非 Windows 默认 MJPEG 兜底）。
    pub codec: dfy_codec::Codec,
}

/// 主控端（View）参数。
#[derive(Debug, Clone)]
pub struct ViewOptions {
    /// Agent 地址。
    pub addr: SocketAddr,
    /// 连接口令。
    pub password: String,
    /// 严格校验 Agent 证书的 SPKI 指纹。
    pub expect_fingerprint: Option<[u8; 32]>,
    /// 观察多久（秒）。
    pub seconds: u64,
    /// 保存第一帧解码结果为裸 BGRA（便于人工核对）。
    pub save_first_frame: Option<PathBuf>,
    /// 是否采集本地鼠标/键盘并发给 Agent（`--control` 开启；仅 Windows）。
    pub control: bool,
    /// 是否与 Agent 双向同步剪贴板（`--clipboard` 开启）。
    pub clipboard: bool,
    /// 经中继接入（`None` = 直连）。
    pub relay: Option<RelayOptions>,
    /// P2P 直连打洞（`None` = 直连/中继）。
    pub p2p: Option<P2POptions>,
    /// 视频编码格式（必须与 Agent 一致）。
    pub codec: dfy_codec::Codec,
}

/// 启动真实 Agent：监听、握手、采集+编码、推流，直到 Ctrl+C。
pub async fn run_agent(opts: AgentOptions) -> Result<()> {
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = &opts;
        anyhow::bail!(
            "当前平台的采集后端尚未实现（v1 支持 Windows DXGI 与 Linux X11；macOS 待做）"
        );
    }

    #[cfg(any(windows, target_os = "linux"))]
    {
        #[cfg(windows)]
        use dfy_capture::DxgiCapturer as PlatformCapturer;
        #[cfg(target_os = "linux")]
        use dfy_capture::LinuxCapturer as PlatformCapturer;

        let monitors = PlatformCapturer::monitors().context("枚举显示器失败")?;
        let monitor = monitors
            .iter()
            .find(|m| m.id == opts.monitor)
            .or_else(|| monitors.first())
            .with_context(|| {
                format!(
                    "找不到显示器索引 {}（共 {} 个）",
                    opts.monitor,
                    monitors.len()
                )
            })?
            .clone();
        // H.264 4:2:0 要求偶数尺寸。
        let width = monitor.w & !1;
        let height = monitor.h & !1;
        anyhow::ensure!(
            width >= 2 && height >= 2,
            "显示器尺寸非法：{}x{}",
            monitor.w,
            monitor.h
        );

        let enc_cfg = dfy_codec::EncoderConfig {
            codec: opts.codec,
            width,
            height,
            fps: opts.fps,
            bitrate_kbps: opts.bitrate_kbps,
            keyframe_interval: opts.fps.saturating_mul(2).max(1),
            prefer_hardware: true,
        };

        // 先创建再丢弃一次编码器：尽早暴露「没有可用编码器」这类环境问题。
        drop(
            MftEncoderAdapter::new(&enc_cfg)
                .with_context(|| format!("创建 {} 编码器失败", enc_cfg.codec))?,
        );

        // 中继模式下：先与中继完成 tag 配对，再把配对后的 socket 交给 quinn；
        // 端点看到的对端是中继地址，TLS/会话语义完全不变。keepalive 由 RelaySocket 自带任务维持，
        // 因此这里要把 socket 句柄持有到函数结束。
        let mut relay_socket = None;
        let (endpoint, identity) = if let Some(relay) = opts.relay.as_ref() {
            let socket = relay.bind(Role::Agent).await.context("连接中继失败")?;
            println!(
                "[agent] 已与中继 {} 配对（tag {}）",
                relay.addr,
                crate::relay_bridge::tag_hex(&relay.tag)
            );
            let identity = TlsIdentity::generate("dfy-agent").context("生成 Agent TLS 身份失败")?;
            let copy = socket.try_clone_socket().context("复制中继 socket 失败")?;
            let endpoint = agent_endpoint_on_socket(copy, identity.clone())
                .context("用中继 socket 创建 Agent QUIC 端点失败")?;
            relay_socket = Some(socket);
            (endpoint, identity)
        } else if let Some(p2p) = opts.p2p.as_ref() {
            // P2P 直连：先按 STUN 结果收集候选并双端打洞，再把已连通的 socket 交给 quinn。
            let handoff = crate::p2p_bridge::prepare("agent", p2p)
                .await
                .context("P2P 打洞失败")?;
            println!(
                "[agent] P2P 就绪：本地端口 {}，候选 {} 个，NAT {:?}",
                handoff.local_port,
                handoff.candidates.len(),
                handoff.nat_hint
            );
            let identity = TlsIdentity::generate("dfy-agent").context("生成 Agent TLS 身份失败")?;
            let endpoint = agent_endpoint_on_socket(handoff.socket, identity.clone())
                .context("用打洞 socket 创建 Agent QUIC 端点失败")?;
            (endpoint, identity)
        } else {
            bind_agent(opts.addr)
                .with_context(|| format!("在 {} 上监听失败（UDP 端口被占用？）", opts.addr))?
        };
        let local = endpoint.local_addr().context("读取监听地址失败")?;
        println!("[agent] 监听 {local}（UDP/QUIC，ALPN = dfyp/1）");
        println!("[agent] 证书 SPKI-SHA256 = {}", identity.fingerprint_hex());
        println!("[agent] 短指纹 = {}", identity.fingerprint_short());
        println!(
            "[agent] 显示器 {}（{} {}x{}，DPI {}）→ {} {}kbps {}fps",
            monitor.id,
            monitor.name,
            width,
            height,
            monitor.dpi,
            enc_cfg.codec,
            opts.bitrate_kbps,
            opts.fps
        );
        println!(
            "[agent] 口令 = {}（MVP；生产应使用 Argon2id/TOTP）",
            opts.password
        );
        println!("[agent] 等待客户端连接，Ctrl+C 退出");

        loop {
            let incoming = tokio::select! {
                incoming = endpoint.accept() => match incoming {
                    Some(incoming) => incoming,
                    None => break,
                },
                _ = tokio::signal::ctrl_c() => {
                    println!("\n[agent] 收到 Ctrl+C，关闭");
                    break;
                }
            };

            let conn = match incoming.await {
                Ok(conn) => conn,
                Err(err) => {
                    warn!(error = %err, "接受连接失败");
                    continue;
                }
            };
            let peer = conn.remote_address();
            println!("[agent] 新连接：{peer}");

            let (welcome, chan) = match dfy_transport::server_handshake(
                &conn,
                &ServerAuth::new(&opts.password, "dfy-agent"),
            )
            .await
            {
                Ok(v) => v,
                Err(err) => {
                    warn!(error = %err, "握手失败，丢弃连接");
                    continue;
                }
            };

            let caps = CaptureLoopConfig {
                capture_timeout_ms: 5,
                max_fps: opts.fps.max(1),
                queue_capacity: 16,
                max_consecutive_errors: 10,
            };
            let monitor_index = monitor.id;
            let enc_cfg_for_loop = enc_cfg.clone();
            let capture = match start_capture_loop(caps, move || {
                let capturer = PlatformCapturer::new(monitor_index)?;
                let encoder = MftEncoderAdapter::new(&enc_cfg_for_loop)?;
                Ok((capturer, encoder))
            }) {
                Ok(capture) => capture,
                Err(err) => {
                    warn!(error = %err, "启动采集线程失败");
                    continue;
                }
            };

            let agent_cfg = AgentConfig {
                max_frames: opts.max_frames,
                idle_timeout: Duration::from_secs(3600),
                drain_delay: Duration::from_millis(100),
            };

            // 每个连接独立的注入处理器：注入失败（无桌面/被拒）只降级为「只串流」，不中断会话。
            let handler = AgentHandler::new(opts.allow_input, opts.allow_clipboard);
            println!(
                "[agent] 输入注入{}",
                if handler.is_enabled() {
                    "已启用"
                } else {
                    "未启用（只串流；--no-input 或后端不可用）"
                }
            );
            println!(
                "[agent] 剪贴板同步{}",
                if handler.clipboard_enabled() {
                    "已启用（拉模式，500ms 轮询）"
                } else {
                    "未启用（--no-clipboard 或后端不可用）"
                }
            );

            let result =
                serve_agent(conn, welcome, chan, capture, handler.clone(), agent_cfg).await;
            // 防卡键：会话结束（正常/异常）立刻释放本机可能仍按下的键。
            handler.release();
            let injected = handler.stats();
            let clipboard = handler.clipboard_stats();
            match result {
                Ok(stats) => println!(
                    "[agent] 会话结束：帧 {}，{}，RTT {:?} ms，原因 {}；输入注入 {} 条（忽略 {}，失败 {}）",
                    stats.frames_sent,
                    dfy_transport::human_bytes(stats.bytes_sent),
                    stats.rtt_ms,
                    stats.end_reason,
                    injected.injected,
                    injected.ignored,
                    injected.failed
                ),
                Err(err) => warn!(error = %err, "会话异常结束"),
            }
            println!(
                "[agent] 剪贴板：清单 {}↑/{}↓，拉取 {}↑/{}↓，分块 {}↑/{}↓，确认 {}↑/{}↓",
                clipboard.lists_sent,
                clipboard.lists_received,
                clipboard.pulls_sent,
                clipboard.pulls_received,
                clipboard.chunks_sent,
                clipboard.chunks_received,
                clipboard.applied_sent,
                clipboard.applied_received
            );
        }

        endpoint.close(0u32.into(), b"agent shutdown");
        drop(relay_socket);
        println!("[agent] 已退出");
        Ok(())
    }
}

/// 主控端统计。
#[derive(Debug, Default, Clone)]
pub struct ViewStats {
    /// 收到的视频 DATAGRAM 数。
    pub datagrams: u64,
    /// 收到的字节数。
    pub bytes: u64,
    /// 重组成完整帧的数目。
    pub frames: u64,
    /// 成功解码的帧数。
    pub decoded: u64,
    /// 关键帧数。
    pub keyframes: u64,
    /// 解析失败数。
    pub malformed: u64,
    /// 心跳 RTT（毫秒）。
    pub rtt_ms: Option<f64>,
    /// 发出的鼠标移动 DATAGRAM 数（仅 --control）。
    pub input_moves: u64,
    /// 发出的其它输入事件数（键/按键/滚轮，仅 --control）。
    pub input_events: u64,
    /// 结束原因。
    pub end_reason: &'static str,
}

/// 连接 Agent，收流并解码 N 秒，打印统计。
pub async fn run_view(opts: ViewOptions) -> Result<ViewStats> {
    let tls = client_tls(opts.expect_fingerprint).context("构造客户端 TLS 失败")?;
    let mut relay_socket = None;
    let (endpoint, conn) = if let Some(relay) = opts.relay.as_ref() {
        let socket = relay.bind(Role::Client).await.context("连接中继失败")?;
        println!(
            "[view] 已与中继 {} 配对（tag {}）",
            relay.addr,
            crate::relay_bridge::tag_hex(&relay.tag)
        );
        let copy = socket.try_clone_socket().context("复制中继 socket 失败")?;
        let pair = connect_on_socket(copy, tls.clone(), relay.addr)
            .await
            .with_context(|| format!("经中继连接 {} 失败（agent 是否已上线？）", relay.addr))?;
        relay_socket = Some(socket);
        pair
    } else if let Some(p2p) = opts.p2p.as_ref() {
        let handoff = crate::p2p_bridge::prepare("view", p2p)
            .await
            .context("P2P 打洞失败")?;
        println!(
            "[view] P2P 就绪：本地端口 {}，候选 {} 个，NAT {:?}",
            handoff.local_port,
            handoff.candidates.len(),
            handoff.nat_hint
        );
        connect_on_socket(handoff.socket, tls.clone(), handoff.peer)
            .await
            .with_context(|| format!("经 P2P 对端 {} 连接失败", handoff.peer))?
    } else {
        connect(opts.addr, tls.clone())
            .await
            .with_context(|| format!("连接 {} 失败（agent 是否已启动/UDP 被拦？）", opts.addr))?
    };
    println!(
        "[view] 已连接 {}",
        if opts.relay.is_some() {
            "（经中继）"
        } else {
            "（直连）"
        }
    );
    if let Some(observed) = tls.observed_hex() {
        println!("[view] Agent 证书 SPKI-SHA256 = {observed}");
    }

    let (welcome, chan) = client_handshake(&conn, &opts.password, "dfy-view", vec![0u8; 32])
        .await
        .context("握手失败")?;
    println!(
        "[view] 握手成功：agent = {}，能力 {} 项，权限位图 0x{:08x}",
        welcome.agent_name,
        welcome.capability.len(),
        welcome.permission_bitmap
    );

    let ClientSession {
        conn,
        welcome: _,
        tx,
        rx,
        heartbeat,
    } = ClientSession::new(conn.clone(), welcome, chan);
    let mut hb = heartbeat;
    let mut tx = tx;
    let mut rx = rx;
    let mut decoder = MftDecoderAdapter::with_codec(opts.codec, true)
        .with_context(|| format!("创建 {} 解码器失败", opts.codec))?;
    let mut assembler = FrameAssembler::new();
    let mut stats = ViewStats::default();
    let deadline = Instant::now() + Duration::from_secs(opts.seconds.max(1));
    let mut saved = false;
    let mut next_ping = Instant::now();

    // 剪贴板同步（默认关闭；--clipboard 开启。平台不支持时自动降级）。
    let mut clipboard = if opts.clipboard {
        ViewClipboard::new()
    } else {
        None
    };
    if clipboard.is_some() {
        println!("[view] 剪贴板同步已启用（拉模式，500ms 轮询本地剪贴板）");
    }
    let mut next_clipboard_poll = Instant::now() + crate::clipboard_bridge::CLIPBOARD_POLL;

    // 控制输入采集（仅 Windows 有实现；不可用时降级为「只收视频」）。
    let mut capture = if opts.control {
        match InputCapture::start() {
            Ok(capture) => {
                println!("[view] 控制输入已启用（鼠标移动 10ms 合并；Ctrl+Alt+Shift+Q 停止）");
                capture
            }
            Err(err) => {
                println!("[view] 控制输入不可用：{err:#}；继续只收视频");
                InputCapture::disabled()
            }
        }
    } else {
        InputCapture::disabled()
    };

    loop {
        if Instant::now() >= deadline {
            stats.end_reason = "duration";
            break;
        }
        tokio::select! {
            biased;

            frame = rx.recv_ctrl() => {
                match frame {
                    Ok(Some(env)) => match &env.body {
                        Some(envelope::Body::Ping(_)) => {
                            if let Some(pong) = hb.on_frame(&env) {
                                let _ = tx.send_ctrl(&pong).await;
                            }
                        }
                        Some(envelope::Body::Pong(_)) => {
                            hb.on_frame(&env);
                        }
                        Some(envelope::Body::Bye(_)) => {
                            stats.end_reason = "agent_bye";
                            break;
                        }
                        _ => {
                            if let Some(sync) = clipboard.as_mut() {
                                let replies = sync.on_ctrl(&env);
                                for reply in replies {
                                    let _ = tx.send_ctrl(&reply).await;
                                }
                            }
                        }
                    },
                    Ok(None) => { stats.end_reason = "ctrl_closed"; break; }
                    Err(_) => { stats.end_reason = "ctrl_error"; break; }
                }
            }

            datagram = conn.read_datagram() => {
                match datagram {
                    Ok(bytes) => {
                        stats.datagrams += 1;
                        stats.bytes += bytes.len() as u64;
                        match dfy_proto::decode_video(&bytes) {
                            Ok((header, payload)) => {
                                if header.flags & dfy_proto::consts::VIDEO_FLAG_KEY != 0 {
                                    stats.keyframes += 1;
                                }
                                match assembler.push(&header, payload) {
                                    Ok(Some(frame)) => {
                                        stats.frames += 1;
                                        match decoder.decode(&frame.data, frame.ts_us as u64) {
                                            Ok(Some(decoded)) => {
                                                stats.decoded += 1;
                                                if !saved {
                                                    if let Some(path) = &opts.save_first_frame {
                                                        let mut buf = Vec::with_capacity(decoded.data.len());
                                                        buf.extend_from_slice(&decoded.data);
                                                        std::fs::write(path, &buf)
                                                            .with_context(|| format!("写 {} 失败", path.display()))?;
                                                        println!("[view] 首帧已保存：{}（{}x{}，{} 字节）",
                                                            path.display(), decoded.width, decoded.height, buf.len());
                                                        saved = true;
                                                    }
                                                }
                                            }
                                            Ok(None) => {}
                                            Err(err) => debug!(error = %err, "解码失败"),
                                        }
                                    }
                                    Ok(None) => {}
                                    Err(err) => {
                                        stats.malformed += 1;
                                        debug!(error = %err, "分片重组失败");
                                    }
                                }
                            }
                            Err(err) => {
                                stats.malformed += 1;
                                debug!(error = %err, "解析视频头失败");
                            }
                        }
                    }
                    Err(_) => { stats.end_reason = "conn_closed"; break; }
                }
            }

            msg = capture.recv() => {
                match msg {
                    Some(CtrlMsg::Move(ev)) => match send_input_move(&conn, &ev) {
                        Ok(()) => stats.input_moves += 1,
                        Err(err) => debug!(error = %err, "发送鼠标移动 DATAGRAM 失败"),
                    },
                    Some(CtrlMsg::Event(env)) => {
                        if tx.send_ctrl(&env).await.is_ok() {
                            stats.input_events += 1;
                        }
                    }
                    Some(CtrlMsg::Stopped) => {
                        println!("[view] 收到 Ctrl+Alt+Shift+Q：停止采集控制输入（继续收视频）");
                        capture.stop();
                    }
                    None => {
                        debug!("控制输入采集已结束");
                        capture.stop();
                    }
                }
            }

            _ = tokio::time::sleep_until(next_clipboard_poll) => {
                next_clipboard_poll = Instant::now() + crate::clipboard_bridge::CLIPBOARD_POLL;
                if let Some(sync) = clipboard.as_mut() {
                    for env in sync.poll() {
                        let _ = tx.send_ctrl(&env).await;
                    }
                }
            }

            _ = tokio::time::sleep_until(next_ping) => {
                let ts = dfy_transport::now_us();
                if tx.send_ctrl(&dfy_proto::Envelope::ping(ts)).await.is_ok() {
                    hb.mark_ping_sent(ts);
                }
                next_ping = Instant::now() + hb.interval;
                if hb.timed_out() {
                    stats.end_reason = "timeout";
                    break;
                }
            }
        }
    }

    capture.stop();
    stats.rtt_ms = hb.rtt_ms();
    let _ = tx
        .send_ctrl(&dfy_proto::Envelope::bye(dfy_proto::BYE_NORMAL))
        .await;
    let _ = tx.finish();
    conn.close(0u32.into(), b"view done");
    endpoint.close(0u32.into(), b"view done");
    drop(relay_socket);
    println!("[view] ---- 统计（{}s）----", opts.seconds.max(1));
    println!(
        "[view] DATAGRAM = {}（{}），重组帧 = {}，解码帧 = {}，关键帧 = {}，解析失败 = {}",
        stats.datagrams,
        dfy_transport::human_bytes(stats.bytes),
        stats.frames,
        stats.decoded,
        stats.keyframes,
        stats.malformed
    );
    println!(
        "[view] RTT = {:?} ms，结束原因 = {}",
        stats.rtt_ms, stats.end_reason
    );
    if opts.control {
        println!(
            "[view] 控制输入：鼠标移动 DATAGRAM = {}，其它事件 = {}",
            stats.input_moves, stats.input_events
        );
    }
    if let Some(sync) = clipboard.as_ref() {
        let c = sync.stats();
        println!(
            "[view] 剪贴板：清单 {}↑/{}↓，拉取 {}↑/{}↓，分块 {}↑/{}↓，确认 {}↑/{}↓，字节 {}↑/{}↓",
            c.lists_sent,
            c.lists_received,
            c.pulls_sent,
            c.pulls_received,
            c.chunks_sent,
            c.chunks_received,
            c.applied_sent,
            c.applied_received,
            dfy_transport::human_bytes(c.bytes_sent),
            dfy_transport::human_bytes(c.bytes_received)
        );
    }
    info!(
        datagrams = stats.datagrams,
        frames = stats.frames,
        decoded = stats.decoded,
        "view 结束"
    );
    Ok(stats)
}

/// 便于测试：把一帧 BGRA 写成 PPM（P6）以便无 GUI 环境下人工查看。
#[allow(dead_code)]
pub fn bgra_to_ppm(frame: &dfy_session::DecodedFrame) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + frame.data.len() / 4 * 3);
    out.extend_from_slice(format!("P6\n{} {}\n255\n", frame.width, frame.height).as_bytes());
    for px in frame.data.chunks_exact(4) {
        out.push(px[2]);
        out.push(px[1]);
        out.push(px[0]);
    }
    out
}

/// 便于其他模块构造原始帧（测试用）。
#[allow(dead_code)]
pub fn blank_frame(width: u32, height: u32) -> RawFrame {
    RawFrame {
        width,
        height,
        stride: width * 4,
        data: vec![0u8; (width * height * 4) as usize],
        pts_us: 0,
        full_frame: true,
    }
}

/// 解析指纹文本的小工具（供 main.rs 复用）。
pub fn parse_fingerprint_arg(text: &str) -> Result<[u8; 32]> {
    parse_fingerprint_hex(text).context("解析证书指纹失败（需要 64 位 hex）")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bgra_to_ppm_header() {
        let frame = dfy_session::DecodedFrame {
            width: 2,
            height: 1,
            stride: 8,
            data: vec![1, 2, 3, 255, 4, 5, 6, 255],
            pts_us: 0,
        };
        let ppm = bgra_to_ppm(&frame);
        assert!(ppm.starts_with(b"P6\n2 1\n255\n"));
        assert_eq!(ppm.len(), 11 + 6);
        assert_eq!(&ppm[11..], &[3, 2, 1, 6, 5, 4]);
    }
}
