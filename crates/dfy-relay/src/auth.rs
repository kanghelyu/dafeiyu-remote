// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Dafeiyu Remote contributors
// Copyright (c) 2026 Dafeiyu Remote contributors
//! bind 鉴权、时间窗与 nonce 防重放。
//!
//! `auth = HMAC-SHA256(relay_key, tag || role || nonce || ts_ms_be)`；
//! `ts_ms` 为客户端毫秒时间戳，服务端允许 ±`bind_window_s`（默认 300s）。

use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::proto::{Auth, Nonce, Tag};

type HmacSha256 = Hmac<Sha256>;

/// 计算 bind 鉴权值。
pub fn compute_auth(key: &[u8], tag: &Tag, role: u8, nonce: &Nonce, ts_ms: u64) -> Auth {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC 接受任意长度 key");
    mac.update(tag);
    mac.update(&[role]);
    mac.update(nonce);
    mac.update(&ts_ms.to_be_bytes());
    let out = mac.finalize().into_bytes();
    let mut auth = [0u8; 32];
    auth.copy_from_slice(&out);
    auth
}

/// 常量时间校验 bind 鉴权值。
pub fn verify_auth(
    key: &[u8],
    tag: &Tag,
    role: u8,
    nonce: &Nonce,
    ts_ms: u64,
    auth: &Auth,
) -> bool {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC 接受任意长度 key");
    mac.update(tag);
    mac.update(&[role]);
    mac.update(nonce);
    mac.update(&ts_ms.to_be_bytes());
    mac.verify_slice(auth).is_ok()
}

/// 时间窗判定：`|ts - now| <= window_s`。
pub fn ts_in_window(ts_ms: u64, now_ms: u64, window_s: u64) -> bool {
    ts_ms.abs_diff(now_ms) <= window_s.saturating_mul(1000)
}

/// 有界 nonce 去重表：仅清理过期记录，满载时拒绝新 nonce。
///
/// 语义：`insert` 返回 `true` 表示已记录新 nonce（可继续 bind），
/// `false` 表示重放或容量已满。不能为接受新请求而淘汰仍在有效期内的记录。
pub struct NonceCache {
    capacity: usize,
    ttl: Duration,
    set: HashSet<(Tag, u8, Nonce)>,
    order: VecDeque<((Tag, u8, Nonce), Instant)>,
}

impl NonceCache {
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            capacity: capacity.max(1),
            ttl,
            set: HashSet::new(),
            order: VecDeque::new(),
        }
    }

    pub fn insert(&mut self, tag: Tag, role: u8, nonce: Nonce, now: Instant) -> bool {
        // 只能清过期记录：容量压力不能缩短有效 nonce 的防重放期限。
        while let Some((key, at)) = self.order.front() {
            let expired = now.saturating_duration_since(*at) > self.ttl;
            if expired {
                self.set.remove(key);
                self.order.pop_front();
            } else {
                break;
            }
        }
        let key = (tag, role, nonce);
        if self.set.contains(&key) {
            return false;
        }
        if self.set.len() >= self.capacity {
            return false;
        }
        self.set.insert(key);
        self.order.push_back((key, now));
        true
    }

    pub fn len(&self) -> usize {
        self.set.len()
    }

    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";

    fn tag(n: u8) -> Tag {
        [n; 16]
    }
    fn nonce(n: u8) -> Nonce {
        [n; 16]
    }

    #[test]
    fn hmac_positive_and_negative() {
        let t = tag(1);
        let n = nonce(2);
        let auth = compute_auth(KEY, &t, 0, &n, 1_000);
        assert!(verify_auth(KEY, &t, 0, &n, 1_000, &auth));
        // 角色/tag/时间戳/nonce 任一变化都必须失败。
        assert!(!verify_auth(KEY, &t, 1, &n, 1_000, &auth));
        assert!(!verify_auth(KEY, &tag(9), 0, &n, 1_000, &auth));
        assert!(!verify_auth(KEY, &t, 0, &nonce(3), 1_000, &auth));
        assert!(!verify_auth(KEY, &t, 0, &n, 1_001, &auth));
        // 错误 key 必须失败。
        assert!(!verify_auth(
            b"ffffffffffffffffffffffffffffffff",
            &t,
            0,
            &n,
            1_000,
            &auth
        ));
    }

    #[test]
    fn timestamp_window() {
        assert!(ts_in_window(1_000_000, 1_000_000, 300));
        assert!(ts_in_window(1_000_000, 1_300_000, 300));
        assert!(!ts_in_window(1_000_000, 1_300_001, 300));
        assert!(!ts_in_window(1_000_000, 699_999, 300));
        assert!(ts_in_window(0, 299_000, 300));
    }

    #[test]
    fn nonce_replay_rejected() {
        let mut c = NonceCache::new(8, Duration::from_secs(300));
        let now = Instant::now();
        assert!(c.insert(tag(1), 0, nonce(1), now));
        assert!(!c.insert(tag(1), 0, nonce(1), now));
        // 不同 role/tag 的同 nonce 是不同记录。
        assert!(c.insert(tag(1), 1, nonce(1), now));
        assert!(c.insert(tag(2), 0, nonce(1), now));
    }

    #[test]
    fn nonce_full_cache_keeps_live_records_and_rejects_new_entries() {
        let mut c = NonceCache::new(1, Duration::from_secs(660));
        let now = Instant::now();
        assert!(c.insert(tag(1), 0, nonce(1), now));
        assert!(
            !c.insert(tag(1), 0, nonce(1), now),
            "满载时不能先淘汰仍有效的最旧 nonce 再接受其重放"
        );
        assert!(!c.insert(tag(2), 1, nonce(2), now));
        assert!(!c.insert(tag(1), 0, nonce(1), now));
        assert_eq!(c.len(), 1);
        // TTL 边界仍保留，只有严格过期之后才能腾出容量。
        assert!(!c.insert(tag(2), 1, nonce(2), now + Duration::from_secs(660)));
        assert!(c.insert(
            tag(2),
            1,
            nonce(2),
            now + Duration::from_secs(660) + Duration::from_nanos(1)
        ));
    }

    #[test]
    fn nonce_capacity_bounded() {
        let mut c = NonceCache::new(4, Duration::from_secs(300));
        let now = Instant::now();
        for i in 0..4u8 {
            assert!(c.insert(tag(1), 0, nonce(i), now));
        }
        for i in 4..10u8 {
            assert!(!c.insert(tag(1), 0, nonce(i), now));
        }
        assert_eq!(c.len(), 4, "nonce 表必须有界");
        // 满载后的重放不能腾出容量，所有仍有效的记录都必须保留。
        for i in 0..4u8 {
            assert!(!c.insert(tag(1), 0, nonce(i), now));
        }
    }

    #[test]
    fn nonce_ttl_eviction() {
        let mut c = NonceCache::new(1024, Duration::from_millis(100));
        let t0 = Instant::now();
        assert!(c.insert(tag(1), 0, nonce(1), t0));
        assert!(!c.insert(tag(1), 0, nonce(1), t0));
        let t1 = t0 + Duration::from_millis(250);
        // TTL 之后旧 nonce 被淘汰。
        assert!(c.insert(tag(1), 0, nonce(1), t1));
    }
}
