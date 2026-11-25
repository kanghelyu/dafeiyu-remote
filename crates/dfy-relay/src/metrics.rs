// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Dafeiyu Remote contributors
// Copyright (c) 2026 Dafeiyu Remote contributors
//! 无依赖的原子计数器 + Prometheus 文本输出。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// 丢弃原因（Prometheus label `reason`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    /// 未 bind 的源地址发来的数据。
    UnknownSource,
    /// 每 tag 限速超出。
    RateLimit,
    /// 全局带宽上限。
    GlobalRateLimit,
    /// tag 两个端点已满（第三端点）。
    TagFull,
    /// bind 鉴权失败。
    Auth,
    /// 时间戳超出时窗。
    Expired,
    /// nonce 重放。
    Replay,
    /// 报文格式非法。
    BadPacket,
    /// 转发失败（socket 错误 / 对端通道关闭）。
    ForwardError,
    /// 已 bind 但配对端点尚未就绪。
    Unpaired,
    /// 会话数达到 max_sessions。
    MaxSessions,
}

impl DropReason {
    pub const ALL: [DropReason; 11] = [
        DropReason::UnknownSource,
        DropReason::RateLimit,
        DropReason::GlobalRateLimit,
        DropReason::TagFull,
        DropReason::Auth,
        DropReason::Expired,
        DropReason::Replay,
        DropReason::BadPacket,
        DropReason::ForwardError,
        DropReason::Unpaired,
        DropReason::MaxSessions,
    ];

    fn idx(self) -> usize {
        self as usize
    }

    pub fn label(self) -> &'static str {
        match self {
            DropReason::UnknownSource => "unknown_source",
            DropReason::RateLimit => "rate_limit",
            DropReason::GlobalRateLimit => "global_rate_limit",
            DropReason::TagFull => "tag_full",
            DropReason::Auth => "auth",
            DropReason::Expired => "expired",
            DropReason::Replay => "replay",
            DropReason::BadPacket => "bad_packet",
            DropReason::ForwardError => "forward_error",
            DropReason::Unpaired => "unpaired",
            DropReason::MaxSessions => "max_sessions",
        }
    }
}

pub struct Metrics {
    started: Instant,
    sessions: AtomicU64,
    bytes_in: AtomicU64,
    bytes_out: AtomicU64,
    packets_in: AtomicU64,
    packets_out: AtomicU64,
    rebinds: AtomicU64,
    dropped: [AtomicU64; 11],
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            sessions: AtomicU64::new(0),
            bytes_in: AtomicU64::new(0),
            bytes_out: AtomicU64::new(0),
            packets_in: AtomicU64::new(0),
            packets_out: AtomicU64::new(0),
            rebinds: AtomicU64::new(0),
            dropped: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    pub fn set_sessions(&self, n: usize) {
        self.sessions.store(n as u64, Ordering::Relaxed);
    }

    pub fn sessions(&self) -> u64 {
        self.sessions.load(Ordering::Relaxed)
    }

    pub fn add_in(&self, bytes: usize) {
        self.bytes_in.fetch_add(bytes as u64, Ordering::Relaxed);
        self.packets_in.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add_out(&self, bytes: usize) {
        self.bytes_out.fetch_add(bytes as u64, Ordering::Relaxed);
        self.packets_out.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add_rebind(&self) {
        self.rebinds.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_drop(&self, reason: DropReason) {
        self.dropped[reason.idx()].fetch_add(1, Ordering::Relaxed);
    }

    pub fn dropped(&self, reason: DropReason) -> u64 {
        self.dropped[reason.idx()].load(Ordering::Relaxed)
    }

    pub fn bytes_in(&self) -> u64 {
        self.bytes_in.load(Ordering::Relaxed)
    }

    pub fn bytes_out(&self) -> u64 {
        self.bytes_out.load(Ordering::Relaxed)
    }

    pub fn rebinds(&self) -> u64 {
        self.rebinds.load(Ordering::Relaxed)
    }

    pub fn uptime_seconds(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    pub fn dropped_sum(&self) -> u64 {
        DropReason::ALL
            .iter()
            .map(|r| self.dropped(*r))
            .sum::<u64>()
    }

    /// Prometheus 文本格式（0.0.4）。
    pub fn render_prometheus(&self) -> String {
        let mut s = String::with_capacity(2048);
        s.push_str("# HELP dfy_relay_sessions 当前活跃 tag 会话数\n");
        s.push_str("# TYPE dfy_relay_sessions gauge\n");
        s.push_str(&format!("dfy_relay_sessions {}\n", self.sessions()));
        s.push_str("# HELP dfy_relay_bytes_total 中继转发字节数\n");
        s.push_str("# TYPE dfy_relay_bytes_total counter\n");
        s.push_str(&format!(
            "dfy_relay_bytes_total{{dir=\"in\"}} {}\n",
            self.bytes_in()
        ));
        s.push_str(&format!(
            "dfy_relay_bytes_total{{dir=\"out\"}} {}\n",
            self.bytes_out()
        ));
        s.push_str("# HELP dfy_relay_packets_total 中继转发报文数\n");
        s.push_str("# TYPE dfy_relay_packets_total counter\n");
        s.push_str(&format!(
            "dfy_relay_packets_total{{dir=\"in\"}} {}\n",
            self.packets_in.load(Ordering::Relaxed)
        ));
        s.push_str(&format!(
            "dfy_relay_packets_total{{dir=\"out\"}} {}\n",
            self.packets_out.load(Ordering::Relaxed)
        ));
        s.push_str("# HELP dfy_relay_dropped_total 丢弃报文数（按原因）\n");
        s.push_str("# TYPE dfy_relay_dropped_total counter\n");
        for r in DropReason::ALL {
            s.push_str(&format!(
                "dfy_relay_dropped_total{{reason=\"{}\"}} {}\n",
                r.label(),
                self.dropped(r)
            ));
        }
        s.push_str("# HELP dfy_relay_rebinds_total 端点地址重绑定数（NAT rebinding）\n");
        s.push_str("# TYPE dfy_relay_rebinds_total counter\n");
        s.push_str(&format!(
            "dfy_relay_rebinds_total {}\n",
            self.rebinds.load(Ordering::Relaxed)
        ));
        s.push_str("# HELP dfy_relay_uptime_seconds 进程运行秒数\n");
        s.push_str("# TYPE dfy_relay_uptime_seconds gauge\n");
        s.push_str(&format!(
            "dfy_relay_uptime_seconds {}\n",
            self.uptime_seconds()
        ));
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_and_render() {
        let m = Metrics::new();
        m.set_sessions(3);
        m.add_in(100);
        m.add_out(90);
        m.add_rebind();
        m.inc_drop(DropReason::UnknownSource);
        m.inc_drop(DropReason::RateLimit);
        m.inc_drop(DropReason::RateLimit);
        assert_eq!(m.sessions(), 3);
        assert_eq!(m.bytes_in(), 100);
        assert_eq!(m.dropped(DropReason::RateLimit), 2);
        assert_eq!(m.dropped_sum(), 3);
        let text = m.render_prometheus();
        assert!(text.contains("dfy_relay_sessions 3"));
        assert!(text.contains("dfy_relay_bytes_total{dir=\"in\"} 100"));
        assert!(text.contains("dfy_relay_dropped_total{reason=\"rate_limit\"} 2"));
        assert!(text.contains("dfy_relay_uptime_seconds"));
        assert!(text.contains("dfy_relay_rebinds_total 1"));
    }
}
