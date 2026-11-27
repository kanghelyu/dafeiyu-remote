// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 渲染统计与窗口标题格式化（纯逻辑，无窗口依赖，可直接单测）。
//!
//! [`RenderStats`] 只记录「已经 present 成功的帧」，因此它的 FPS/耗时反映的是**实际上屏**
//! 的节奏，而不是解码器收帧的节奏。统计滑动窗口固定为 [`FPS_WINDOW`]，避免长时间运行后
//! 平均值被历史拖住；窗口标题的刷新另按 [`TITLE_REFRESH_INTERVAL`] 节流，防止每个重绘都
//! 调一次 `set_title` 引发窗口管理器抖动。

use std::time::{Duration, Instant};

/// FPS 统计滑动窗口长度（1 秒）。
pub const FPS_WINDOW: Duration = Duration::from_secs(1);

/// 窗口标题里统计信息的刷新间隔（500ms）。
pub const TITLE_REFRESH_INTERVAL: Duration = Duration::from_millis(500);

/// 上屏统计累计器。
///
/// 典型用法：UI 线程每渲染完成一帧就调用 [`RenderStats::record_render`]；需要更新标题时
/// 先问 [`RenderStats::should_refresh_title`]（自带节流），再用 [`RenderStats::title`] 取标题。
#[derive(Debug, Clone, Default)]
pub struct RenderStats {
    total_frames: u64,
    window_start: Option<Instant>,
    window_frames: u64,
    fps: Option<f64>,
    last_render_time: Duration,
    last_frame_size: Option<(u32, u32)>,
    last_title_at: Option<Instant>,
}

impl RenderStats {
    /// 记录一次**已成功上屏**的渲染。
    ///
    /// - `now`：本次渲染完成的时间（注入 `Instant` 便于单测，不依赖真实时钟）。
    /// - `frame_size`：本帧的远端画面尺寸。
    /// - `render_time`：从开始绘制到 `present` 返回的耗时。
    ///
    /// FPS 在累计满 [`FPS_WINDOW`] 之前保持 `None`（不做「第 1 帧就猜 FPS」的假数据）；
    /// 满窗口后按「窗口内帧数 / 窗口秒数」更新，并重置窗口。
    pub fn record_render(&mut self, now: Instant, frame_size: (u32, u32), render_time: Duration) {
        self.total_frames = self.total_frames.saturating_add(1);
        self.window_frames = self.window_frames.saturating_add(1);
        self.last_render_time = render_time;
        self.last_frame_size = Some(frame_size);

        match self.window_start {
            None => self.window_start = Some(now),
            Some(start) => {
                let elapsed = now.saturating_duration_since(start);
                if elapsed >= FPS_WINDOW {
                    let secs = elapsed.as_secs_f64();
                    if secs > 0.0 {
                        self.fps = Some(self.window_frames as f64 / secs);
                    }
                    self.window_start = Some(now);
                    self.window_frames = 0;
                }
            }
        }
    }

    /// 累计上屏帧数（含被窗口遮挡/最小化跳过渲染的帧不计——只有真正 present 的帧算数）。
    #[must_use]
    pub const fn frames(&self) -> u64 {
        self.total_frames
    }

    /// 最近一个滑动窗口的 FPS；累计不足 1 秒时为 `None`。
    #[must_use]
    pub const fn fps(&self) -> Option<f64> {
        self.fps
    }

    /// 最近一帧的渲染耗时（绘制 + present 返回值）。
    #[must_use]
    pub const fn last_render_time(&self) -> Duration {
        self.last_render_time
    }

    /// 最近一帧的远端画面尺寸；还没渲染过任何帧时为 `None`。
    #[must_use]
    pub const fn last_frame_size(&self) -> Option<(u32, u32)> {
        self.last_frame_size
    }

    /// 距上次刷新标题是否已满 [`TITLE_REFRESH_INTERVAL`]（首次调用返回 `true`）。
    ///
    /// 这里会**记下本次刷新时间**：调用方拿到 `true` 后应当立刻调用 `set_title`。
    pub fn should_refresh_title(&mut self, now: Instant) -> bool {
        match self.last_title_at {
            None => {
                self.last_title_at = Some(now);
                true
            }
            Some(last) => {
                if now.saturating_duration_since(last) >= TITLE_REFRESH_INTERVAL {
                    self.last_title_at = Some(now);
                    true
                } else {
                    false
                }
            }
        }
    }

    /// 生成窗口标题。
    ///
    /// - `show_stats == false`、或还没有渲染过任何一帧时：原样返回 `base`。
    /// - 否则追加 ` — {宽}×{高} · {fps} fps · {耗时} ms`；FPS 未满一个统计窗口时显示
    ///   `fps 统计中`，避免展示假数据。
    #[must_use]
    pub fn title(&self, base: &str, show_stats: bool) -> String {
        if !show_stats {
            return base.to_string();
        }
        let Some((width, height)) = self.last_frame_size else {
            return base.to_string();
        };
        let fps = match self.fps {
            Some(fps) => format!("{fps:.1} fps"),
            None => "fps 统计中".to_string(),
        };
        let millis = self.last_render_time.as_secs_f64() * 1000.0;
        format!("{base} — {width}×{height} · {fps} · {millis:.1} ms")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_empty() {
        let stats = RenderStats::default();
        assert_eq!(stats.frames(), 0);
        assert_eq!(stats.fps(), None);
        assert_eq!(stats.last_frame_size(), None);
        assert_eq!(stats.last_render_time(), Duration::ZERO);
        assert_eq!(stats.title("dfy", true), "dfy");
    }

    #[test]
    fn record_render_updates_last_frame_and_total() {
        let now = Instant::now();
        let mut stats = RenderStats::default();
        stats.record_render(now, (1280, 720), Duration::from_millis(7));
        stats.record_render(now, (1920, 1080), Duration::from_millis(3));
        assert_eq!(stats.frames(), 2);
        assert_eq!(stats.last_frame_size(), Some((1920, 1080)));
        assert_eq!(stats.last_render_time(), Duration::from_millis(3));
        // 不足 1 秒窗口：不猜 FPS。
        assert_eq!(stats.fps(), None);
    }

    #[test]
    fn fps_appears_after_one_full_window() {
        let now = Instant::now();
        let mut stats = RenderStats::default();
        stats.record_render(now, (1280, 720), Duration::ZERO);
        // 窗口内又来了 29 帧（合计 30 帧 / 1 秒 = 30 fps）。
        for i in 1..30u32 {
            stats.record_render(
                now + Duration::from_millis(u64::from(i) * 1000 / 30),
                (1280, 720),
                Duration::ZERO,
            );
        }
        assert_eq!(stats.fps(), None, "还差最后一帧才满 1 秒");
        stats.record_render(now + Duration::from_secs(1), (1280, 720), Duration::ZERO);
        let fps = stats.fps().expect("满一个窗口后必须有 FPS");
        assert!((fps - 31.0).abs() < 0.5, "实际 fps={fps}");
        assert_eq!(stats.frames(), 31);
    }

    #[test]
    fn title_includes_only_when_requested() {
        let now = Instant::now();
        let mut stats = RenderStats::default();
        stats.record_render(now, (1280, 720), Duration::from_millis(4));
        // 未开启统计：标题就是 base。
        assert_eq!(stats.title("大肥鱼远程", false), "大肥鱼远程");
        // 开启统计但 FPS 还没满窗口：给出「统计中」，不展示假的 0.0。
        let title = stats.title("大肥鱼远程", true);
        assert!(title.starts_with("大肥鱼远程 — 1280×720"), "{title}");
        assert!(title.contains("fps 统计中"), "{title}");
        assert!(title.contains("4.0 ms"), "{title}");
    }

    #[test]
    fn title_formats_fps_after_window_filled() {
        let now = Instant::now();
        let mut stats = RenderStats::default();
        stats.record_render(now, (640, 480), Duration::ZERO);
        stats.record_render(now + Duration::from_secs(1), (640, 480), Duration::ZERO);
        let title = stats.title("dfy", true);
        assert!(title.contains("640×480"), "{title}");
        // 2 帧 / 1 秒 = 2 fps。
        assert!(title.contains("2.0 fps"), "{title}");
    }

    #[test]
    fn title_refresh_is_throttled() {
        let now = Instant::now();
        let mut stats = RenderStats::default();
        assert!(stats.should_refresh_title(now), "首次必须刷新");
        assert!(!stats.should_refresh_title(now), "同一时刻不重复刷新");
        assert!(!stats.should_refresh_title(now + TITLE_REFRESH_INTERVAL / 2));
        assert!(stats.should_refresh_title(now + TITLE_REFRESH_INTERVAL));
        assert!(!stats.should_refresh_title(now + TITLE_REFRESH_INTERVAL));
    }
}
