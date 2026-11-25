// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Dafeiyu Remote contributors
// Copyright (c) 2026 Dafeiyu Remote contributors
//! 令牌桶限速（每 tag / 全局共用同一实现）。

use std::time::Instant;

/// 字节速率令牌桶。
#[derive(Debug, Clone)]
pub struct TokenBucket {
    rate: f64,
    capacity: f64,
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    /// `rate_bytes_per_s` 为持续速率，`burst_bytes` 为桶容量（突发上限）。
    pub fn new(rate_bytes_per_s: f64, burst_bytes: f64, now: Instant) -> Self {
        let capacity = burst_bytes.max(1.0);
        Self {
            rate: rate_bytes_per_s.max(0.0),
            capacity,
            tokens: capacity,
            last: now,
        }
    }

    /// 由 Mbps 构造：容量取 100ms 的流量（至少 64 KiB，避免小包被误伤）。
    pub fn from_mbps(mbps: f64, now: Instant) -> Self {
        let rate = mbps.max(0.0) * 1_000_000.0 / 8.0;
        let burst = (rate * 0.1).max(64.0 * 1024.0);
        Self::new(rate, burst, now)
    }

    fn refill(&mut self, now: Instant) {
        let dt = now.saturating_duration_since(self.last).as_secs_f64();
        if dt > 0.0 {
            self.tokens = (self.tokens + dt * self.rate).min(self.capacity);
            self.last = now;
        }
    }

    /// 尝试消费 `bytes` 字节；不足则返回 false（调用方丢弃并计数）。
    pub fn try_consume(&mut self, bytes: usize, now: Instant) -> bool {
        self.refill(now);
        let need = bytes as f64;
        if self.tokens >= need {
            self.tokens -= need;
            true
        } else {
            false
        }
    }

    pub fn rate_bytes_per_s(&self) -> f64 {
        self.rate
    }

    pub fn available(&self) -> f64 {
        self.tokens
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn bucket_allows_then_rejects() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new(1_000.0, 1_000.0, t0);
        assert!(b.try_consume(800, t0));
        assert!(!b.try_consume(800, t0));
        assert!(b.try_consume(200, t0));
    }

    #[test]
    fn bucket_refills_over_time() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new(1_000.0, 1_000.0, t0);
        assert!(b.try_consume(1_000, t0));
        assert!(!b.try_consume(500, t0));
        let t1 = t0 + Duration::from_secs(1);
        // 1 秒补 1000 字节，但不超过容量 1000。
        assert!(b.try_consume(500, t1));
        assert!(b.available() <= 1_000.0);
    }

    #[test]
    fn bucket_from_mbps_burst() {
        let t0 = Instant::now();
        // 200 Mbps = 25 MB/s；容量 2.5 MB。
        let mut b = TokenBucket::from_mbps(200.0, t0);
        assert!(b.try_consume(1_000_000, t0));
        assert!(b.try_consume(1_000_000, t0));
        // 2.5MB 桶装不下第三个 1MB。
        assert!(!b.try_consume(1_000_000, t0));
    }

    #[test]
    fn zero_rate_drops_everything_after_burst() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new(0.0, 1024.0, t0);
        assert!(b.try_consume(1024, t0));
        assert!(!b.try_consume(1, t0 + Duration::from_secs(10)));
    }
}
