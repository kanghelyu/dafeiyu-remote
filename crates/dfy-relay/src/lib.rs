// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Dafeiyu Remote contributors
// Copyright (c) 2026 Dafeiyu Remote contributors
//! # dfy-relay — DFYP/1 中继服务器（大肥鱼远程）
//!
//! 只做 `tag → (端点A, 端点B)` 的成对转发：不解析 QUIC、不终结 TLS，
//! 端到端加密由客户端负责。数据面支持 UDP（主）与 TCP 降级通道（可互通桥接）。
//!
//! 模块：
//! - [`proto`]：信封协议（Bind / BindOk / BindErr / Bye / TCP 帧）；
//! - [`auth`]：HMAC-SHA256 bind 鉴权、±300s 时间窗、nonce 防重放；
//! - [`state`]：tag 会话表（配对、重绑定、GC、限速）；
//! - [`udp`] / [`tcp`]：数据面收包循环；[`admin`]：本机 HTTP 健康/指标；
//! - [`config`]：TOML 配置；[`selftest`]：进程内端到端自测；
//! - [`relay`]：把上述部件组装成可启动/可优雅关闭的服务。

pub mod admin;
pub mod auth;
pub mod config;
pub mod metrics;
pub mod proto;
pub mod ratelimit;
pub mod relay;
pub mod selftest;
pub mod state;
pub mod tcp;
pub mod udp;

use std::time::{SystemTime, UNIX_EPOCH};

/// 当前 Unix 毫秒时间戳（bind 时间窗与 BindOk.server_ms 使用）。
pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
