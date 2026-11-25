// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 采样值与滑动窗口。
//!
//! 调用方每拍（建议 250ms）把一条路径的原始观测包成 [`PathSample`] 喂进
//! [`SampleWindow`]；窗口用环形缓冲保留最近 N 条（默认 32），并给出：
//! - 平滑 RTT：EWMA（默认 α=0.2）；
//! - 丢包率：窗口内算术平均（ppm）；
//! - 有效接收速率：窗口内算术平均（单位随调用方，本 crate 只做统计）；
//! - 「无进展」时长：最近一条采样的 `stall_ms`。
//!
//! 非法值不会 panic：非有限或负的 RTT 视为「本次没有 RTT 观测」，跳过 EWMA 与均值；
//! 丢包 ppm 截断到 100 万；窗口容量 0 视为 1；时间倒退由 `saturating_sub` 兜底。

/// RTT 的物理上限（毫秒）。更大的值按此截断，避免 f32 参与运算时溢出。
pub const MAX_RTT_MS: f32 = 60_000.0;
/// 丢包率上限（ppm，即 100%）。
pub const MAX_LOSS_PPM: u32 = 1_000_000;

/// 一次路径质量采样。时间由调用方给（`at_ms`，毫秒单调时钟），本 crate 不读系统时钟。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PathSample {
    /// 平滑前的一次 RTT 观测（毫秒）；`NaN` / 负数表示本次没有有效 RTT。
    pub rtt_ms: f32,
    /// 丢包率（百万分之几，0..=1_000_000）。
    pub loss_ppm: u32,
    /// 有效接收速率（bit/s；单位由调用方约定，本 crate 只做窗口统计）。
    pub recv_bps: u64,
    /// 距上一次画面/数据进展的毫秒数（0 表示刚刚有进展）。
    pub stall_ms: u64,
    /// 采样时间戳（毫秒，调用方单调时钟）。
    pub at_ms: u64,
}

impl PathSample {
    /// 返回清洗后的采样，保证后续统计不会因非法值 panic 或污染：
    /// - RTT：非有限或负数 → `NaN`（视为无有效观测）；有限值截断到 `[0, 60_000]`；
    /// - 丢包 ppm：截断到 100 万；
    /// - 接收速率、`stall_ms`、`at_ms` 原样保留。
    ///
    /// 该函数是幂等的（`x.clamped().clamped() == x.clamped()`）。
    pub fn clamped(self) -> Self {
        let rtt_ms = if self.rtt_ms.is_finite() {
            if self.rtt_ms < 0.0 {
                f32::NAN
            } else {
                self.rtt_ms.min(MAX_RTT_MS)
            }
        } else {
            f32::NAN
        };
        Self {
            rtt_ms,
            loss_ppm: self.loss_ppm.min(MAX_LOSS_PPM),
            recv_bps: self.recv_bps,
            stall_ms: self.stall_ms,
            at_ms: self.at_ms,
        }
    }

    /// 本次采样是否有有效 RTT（有限且 ≥0）。
    pub fn has_rtt(&self) -> bool {
        self.rtt_ms.is_finite() && self.rtt_ms >= 0.0
    }
}

/// 滑动窗口的聚合结果（[`SampleWindow::stats`]）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowStats {
    /// 窗口内采样个数。
    pub samples: usize,
    /// 平滑 RTT（EWMA）；窗口内没有有效 RTT 时为 `NaN`。
    pub smoothed_rtt_ms: f32,
    /// 窗口内有效 RTT 的算术平均；窗口内没有有效 RTT 时为 `NaN`。
    pub mean_rtt_ms: f32,
    /// 窗口内丢包率的算术平均（ppm）。
    pub loss_ppm: u32,
    /// 窗口内有效接收速率的算术平均（单位同 [`PathSample::recv_bps`]）。
    pub recv_bps: u64,
    /// 最近一条采样的「无进展」时长（毫秒）。
    pub last_stall_ms: u64,
    /// 最新采样的时间戳（毫秒）。
    pub newest_at_ms: u64,
}

/// 定长环形缓冲 + 在线 EWMA。容量与 α 在创建时固定，`push` 为 O(1)。
#[derive(Debug, Clone)]
pub struct SampleWindow {
    capacity: usize,
    /// 已写入的采样；未满时按下标 0..len 顺序存放，满后 `head` 指向最旧一条。
    slots: Vec<PathSample>,
    head: usize,
    len: usize,
    alpha: f32,
    ewma_rtt: f32,
    has_ewma: bool,
}

impl SampleWindow {
    /// 创建窗口。`capacity == 0` 视为 1；`ewma_alpha` 非法（非有限、≤0 或 >1）时回退到 0.2。
    pub fn new(capacity: usize, ewma_alpha: f32) -> Self {
        let capacity = capacity.max(1);
        let alpha = if ewma_alpha.is_finite() && ewma_alpha > 0.0 && ewma_alpha <= 1.0 {
            ewma_alpha
        } else {
            0.2
        };
        Self {
            capacity,
            slots: Vec::with_capacity(capacity),
            head: 0,
            len: 0,
            alpha,
            ewma_rtt: 0.0,
            has_ewma: false,
        }
    }

    /// 窗口容量（≥1）。
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// 当前窗口内采样个数。
    pub fn len(&self) -> usize {
        self.len
    }

    /// 窗口是否为空。
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// EWMA 系数。
    pub fn alpha(&self) -> f32 {
        self.alpha
    }

    /// 最近一条采样。
    pub fn latest(&self) -> Option<&PathSample> {
        if self.len == 0 {
            None
        } else {
            Some(&self.slots[(self.head + self.len - 1) % self.capacity])
        }
    }

    /// 按时间顺序（最旧 → 最新）遍历窗口内采样。
    pub fn iter(&self) -> impl Iterator<Item = &PathSample> + '_ {
        (0..self.len).map(move |i| &self.slots[(self.head + i) % self.capacity])
    }

    /// 压入一条采样（内部先 [`PathSample::clamped`]）；满时覆盖最旧一条。
    pub fn push(&mut self, sample: PathSample) {
        let sample = sample.clamped();
        if self.len < self.capacity {
            self.slots.push(sample);
            self.len += 1;
        } else {
            self.slots[self.head] = sample;
            self.head = (self.head + 1) % self.capacity;
        }
        if sample.has_rtt() {
            let rtt = sample.rtt_ms;
            self.ewma_rtt = if self.has_ewma {
                self.alpha * rtt + (1.0 - self.alpha) * self.ewma_rtt
            } else {
                rtt
            };
            self.has_ewma = true;
        }
    }

    /// 清空窗口（切换路径后新连接 = 新测量上下文时调用）。
    pub fn reset(&mut self) {
        self.slots.clear();
        self.head = 0;
        self.len = 0;
        self.ewma_rtt = 0.0;
        self.has_ewma = false;
    }

    /// 当前 EWMA RTT；尚无有效 RTT 采样时为 `None`。
    pub fn ewma_rtt(&self) -> Option<f32> {
        self.has_ewma.then_some(self.ewma_rtt)
    }

    /// 计算窗口聚合统计；空窗口返回 `None`。
    pub fn stats(&self) -> Option<WindowStats> {
        if self.len == 0 {
            return None;
        }
        let mut rtt_sum = 0.0f32;
        let mut rtt_count = 0u32;
        let mut loss_sum = 0u64;
        let mut recv_sum = 0u128;
        let mut newest: Option<&PathSample> = None;
        for sample in self.iter() {
            if sample.has_rtt() {
                rtt_sum += sample.rtt_ms;
                rtt_count += 1;
            }
            loss_sum += u64::from(sample.loss_ppm);
            recv_sum += u128::from(sample.recv_bps);
            newest = Some(sample);
        }
        let newest = newest?;
        let n = self.len as u64;
        let mean_rtt_ms = if rtt_count == 0 {
            f32::NAN
        } else {
            rtt_sum / rtt_count as f32
        };
        let loss_ppm = (loss_sum / n).min(u64::from(MAX_LOSS_PPM)) as u32;
        let recv_bps = (recv_sum / u128::from(n)).min(u128::from(u64::MAX)) as u64;
        Some(WindowStats {
            samples: self.len,
            smoothed_rtt_ms: if self.has_ewma { self.ewma_rtt } else { f32::NAN },
            mean_rtt_ms,
            loss_ppm,
            recv_bps,
            last_stall_ms: newest.stall_ms,
            newest_at_ms: newest.at_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(at_ms: u64, rtt_ms: f32, loss_ppm: u32, stall_ms: u64) -> PathSample {
        PathSample {
            rtt_ms,
            loss_ppm,
            recv_bps: 8_000_000,
            stall_ms,
            at_ms,
        }
    }

    fn window(capacity: usize) -> SampleWindow {
        SampleWindow::new(capacity, 0.2)
    }

    #[test]
    fn ring_buffer_keeps_newest_and_reports_window_mean() {
        let mut w = window(3);
        for (i, rtt) in [10.0f32, 20.0, 30.0].iter().enumerate() {
            w.push(sample(i as u64 * 100, *rtt, 0, 0));
        }
        assert_eq!(w.len(), 3);
        w.push(sample(300, 40.0, 0, 0));
        assert_eq!(w.len(), 3);
        assert_eq!(w.latest().map(|s| s.rtt_ms), Some(40.0));
        let stats = w.stats().unwrap();
        assert_eq!(stats.samples, 3);
        assert!((stats.mean_rtt_ms - 30.0).abs() < 0.01);
        let order: Vec<f32> = w.iter().map(|s| s.rtt_ms).collect();
        assert_eq!(order, vec![20.0, 30.0, 40.0]);
    }

    #[test]
    fn ewma_uses_alpha_and_ignores_missing_rtt() {
        let mut w = window(8);
        w.push(sample(0, 100.0, 0, 0));
        w.push(sample(100, 50.0, 0, 0));
        // 0.2*50 + 0.8*100 = 90
        assert!((w.ewma_rtt().unwrap() - 90.0).abs() < 0.001);
        w.push(sample(200, 200.0, 0, 0));
        // 0.2*200 + 0.8*90 = 112
        assert!((w.ewma_rtt().unwrap() - 112.0).abs() < 0.001);
        let before = w.ewma_rtt().unwrap();
        w.push(sample(300, f32::NAN, 0, 0));
        w.push(sample(400, f32::INFINITY, 0, 0));
        assert_eq!(w.ewma_rtt().unwrap(), before);
        let stats = w.stats().unwrap();
        assert!((stats.mean_rtt_ms - 116.666_67).abs() < 0.01);
        assert!(!stats.smoothed_rtt_ms.is_nan());
    }

    #[test]
    fn window_stats_average_loss_and_saturate_recv_rate() {
        let mut w = window(4);
        w.push(PathSample {
            rtt_ms: 10.0,
            loss_ppm: 10_000,
            recv_bps: u64::MAX,
            stall_ms: 0,
            at_ms: 1,
        });
        w.push(PathSample {
            rtt_ms: 10.0,
            loss_ppm: 20_000,
            recv_bps: u64::MAX,
            stall_ms: 0,
            at_ms: 2,
        });
        let stats = w.stats().unwrap();
        assert_eq!(stats.loss_ppm, 15_000);
        assert_eq!(stats.recv_bps, u64::MAX);
        assert_eq!(w.stats().unwrap().samples, 2);
    }

    #[test]
    fn last_stall_comes_from_newest_sample() {
        let mut w = window(4);
        w.push(sample(1, 10.0, 0, 0));
        w.push(sample(2, 10.0, 0, 2_500));
        assert_eq!(w.stats().unwrap().last_stall_ms, 2_500);
    }

    #[test]
    fn capacity_zero_is_normalized_to_one() {
        let mut w = SampleWindow::new(0, 0.2);
        assert_eq!(w.capacity(), 1);
        w.push(sample(1, 10.0, 0, 0));
        w.push(sample(2, 20.0, 0, 0));
        assert_eq!(w.len(), 1);
        assert_eq!(w.latest().unwrap().rtt_ms, 20.0);
    }

    #[test]
    fn invalid_alpha_falls_back_to_default() {
        assert!((SampleWindow::new(4, f32::NAN).alpha() - 0.2).abs() < 0.001);
        assert!((SampleWindow::new(4, 0.0).alpha() - 0.2).abs() < 0.001);
        assert!((SampleWindow::new(4, 1.5).alpha() - 0.2).abs() < 0.001);
        assert!((SampleWindow::new(4, 1.0).alpha() - 1.0).abs() < 0.001);
    }

    #[test]
    fn reset_clears_window_and_ewma() {
        let mut w = window(4);
        w.push(sample(1, 10.0, 0, 0));
        w.reset();
        assert!(w.is_empty());
        assert!(w.latest().is_none());
        assert!(w.stats().is_none());
        assert!(w.ewma_rtt().is_none());
    }

    #[test]
    fn loss_is_clamped_to_one_million_ppm() {
        let mut w = window(2);
        w.push(sample(1, 10.0, u32::MAX, 0));
        assert_eq!(w.stats().unwrap().loss_ppm, MAX_LOSS_PPM);
    }

    #[test]
    fn empty_window_stats_is_none_and_single_sample_is_exact() {
        let w = window(4);
        assert!(w.stats().is_none());
        let mut w = window(4);
        w.push(sample(7, 33.0, 0, 0));
        let stats = w.stats().unwrap();
        assert_eq!(stats.smoothed_rtt_ms, 33.0);
        assert_eq!(stats.mean_rtt_ms, 33.0);
        assert_eq!(stats.newest_at_ms, 7);
    }

    #[test]
    fn sample_clamping_handles_non_finite_and_extremes() {
        let s = PathSample {
            rtt_ms: -1.0,
            loss_ppm: u32::MAX,
            recv_bps: 1,
            stall_ms: 0,
            at_ms: 0,
        };
        let c = s.clamped();
        assert!(!c.has_rtt());
        assert!(c.rtt_ms.is_nan());
        assert_eq!(c.loss_ppm, MAX_LOSS_PPM);
        let again = c.clamped();
        assert!(again.rtt_ms.is_nan());
        assert_eq!(again.loss_ppm, c.loss_ppm);
        assert_eq!(again.recv_bps, c.recv_bps);
        assert_eq!(again.stall_ms, c.stall_ms);
        assert_eq!(again.at_ms, c.at_ms);

        let huge = PathSample {
            rtt_ms: 1.0e30,
            loss_ppm: 0,
            recv_bps: 0,
            stall_ms: 0,
            at_ms: 0,
        };
        assert_eq!(huge.clamped().rtt_ms, MAX_RTT_MS);
    }
}