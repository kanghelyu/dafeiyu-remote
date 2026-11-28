// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0

use std::time::{Duration, Instant};

use crate::frame::{self, Rect};

const MIN_TOOLBAR_HEIGHT: u32 = 32;
const MAX_TOOLBAR_HEIGHT: u32 = 56;
const AUTO_HIDE_DELAY: Duration = Duration::from_secs(2);
pub(crate) const MIN_CLIENT_SIZE: (f64, f64) = (640.0, 360.0);
const BAR_MARGIN: f64 = 6.0;
const BAR_PADDING: f64 = 8.0;
const CONTROL_GAP: f64 = 4.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolbarAction {
    Fullscreen,
    ToggleInput,
    Stats,
    Disconnect,
}

impl ToolbarAction {
    pub(crate) const ALL: [Self; 4] = [
        Self::Fullscreen,
        Self::ToggleInput,
        Self::Stats,
        Self::Disconnect,
    ];

    fn label(self, fullscreen: bool, paused: bool, compact: bool) -> &'static str {
        match self {
            Self::Fullscreen if compact => "全屏",
            Self::Fullscreen if fullscreen => "窗口",
            Self::Fullscreen => "全屏",
            Self::ToggleInput if compact => {
                if paused {
                    "继续"
                } else {
                    "暂停"
                }
            }
            Self::ToggleInput if paused => "继续控制",
            Self::ToggleInput => "暂停控制",
            Self::Stats if compact => "统计",
            Self::Stats => "连接统计",
            Self::Disconnect if compact => "断开",
            Self::Disconnect => "断开连接",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct InputGate {
    paused: bool,
    overlay: bool,
}

impl InputGate {
    pub(crate) fn allows_remote_input(&self) -> bool {
        !self.paused && !self.overlay
    }

    pub(crate) fn set_overlay(&mut self, overlay: bool) -> bool {
        let changed = self.overlay != overlay;
        self.overlay = overlay;
        changed
    }

    pub(crate) fn toggle_paused(&mut self) -> bool {
        self.paused = !self.paused;
        self.paused
    }

    pub(crate) const fn paused(&self) -> bool {
        self.paused
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ToolbarState {
    fullscreen: bool,
    visible: bool,
    last_activity: Option<Instant>,
    hovered: bool,
    focused: bool,
    pressed: Option<ToolbarAction>,
}

impl ToolbarState {
    pub(crate) fn new(fullscreen: bool) -> Self {
        Self {
            fullscreen,
            visible: true,
            last_activity: None,
            hovered: false,
            focused: false,
            pressed: None,
        }
    }

    pub(crate) fn set_fullscreen(&mut self, fullscreen: bool, now: Instant) {
        self.fullscreen = fullscreen;
        self.visible = true;
        self.focused = false;
        self.pressed = None;
        self.last_activity = if fullscreen { Some(now) } else { None };
    }

    pub(crate) fn reveal(&mut self, now: Instant) -> bool {
        let changed = !self.visible;
        self.visible = true;
        self.last_activity = if self.fullscreen { Some(now) } else { None };
        changed
    }

    pub(crate) fn set_hovered(&mut self, hovered: bool, now: Instant) {
        self.hovered = hovered;
        if hovered {
            self.reveal(now);
        } else {
            self.reset_activity(now);
        }
    }

    pub(crate) fn set_focused(&mut self, focused: bool, now: Instant) {
        self.focused = focused;
        if focused {
            self.reveal(now);
        } else {
            self.reset_activity(now);
        }
    }

    pub(crate) fn set_pressed(&mut self, pressed: Option<ToolbarAction>, now: Instant) {
        self.pressed = pressed;
        if pressed.is_some() {
            self.reveal(now);
        } else {
            self.reset_activity(now);
        }
    }

    pub(crate) fn update(&mut self, now: Instant) -> bool {
        if !self.fullscreen {
            self.visible = true;
            self.last_activity = None;
            return false;
        }
        if !self.visible || self.hovered || self.focused || self.pressed.is_some() {
            return false;
        }
        let expired = self
            .last_activity
            .is_some_and(|last| now.saturating_duration_since(last) >= AUTO_HIDE_DELAY);
        if expired {
            self.visible = false;
            self.last_activity = None;
            return true;
        }
        false
    }

    pub(crate) fn next_wakeup(&self) -> Option<Instant> {
        if !self.fullscreen
            || !self.visible
            || self.hovered
            || self.focused
            || self.pressed.is_some()
        {
            return None;
        }
        self.last_activity.map(|last| last + AUTO_HIDE_DELAY)
    }

    pub(crate) const fn visible(&self) -> bool {
        self.visible
    }

    pub(crate) const fn focused(&self) -> bool {
        self.focused
    }

    fn reset_activity(&mut self, now: Instant) {
        self.last_activity =
            if self.fullscreen && !self.hovered && !self.focused && self.pressed.is_none() {
                Some(now)
            } else {
                None
            };
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ToolbarLayout {
    content: Option<Rect>,
    bar: Rect,
    controls: [Option<Rect>; 4],
    status: Option<Rect>,
    summary: Option<Rect>,
    stats: Option<Rect>,
    paused_banner: Option<Rect>,
    font_scale: u32,
    stats_font_scale: u32,
    compact: bool,
    toolbar_visible: bool,
}

impl ToolbarLayout {
    pub(crate) fn new(
        width: u32,
        height: u32,
        scale_factor: f64,
        paused: bool,
        stats_visible: bool,
        toolbar_visible: bool,
    ) -> Self {
        let scale = valid_scale(scale_factor);
        let logical_w = f64::from(width) / scale;
        let logical_h = f64::from(height) / scale;
        let toolbar_height = f64::from(toolbar_height(logical_w));
        let compact = logical_w < 1080.0;
        let font_scale = scale_pixels(if compact { 12.0 } else { 13.0 }, scale).max(1);
        let bar_w = (logical_w - 2.0 * BAR_MARGIN).max(0.0);
        let bar_h = toolbar_height.min((logical_h - 2.0 * BAR_MARGIN).max(0.0));
        let bar = physical_rect(BAR_MARGIN, BAR_MARGIN, bar_w, bar_h, scale, width, height);
        let content = content_rect(width, height);

        let desired = if compact {
            [68.0, 68.0, 60.0, 60.0]
        } else {
            [88.0, 104.0, 96.0, 96.0]
        };
        let available = (bar_w - 2.0 * BAR_PADDING).max(0.0);
        let desired_total: f64 = desired.iter().sum();
        let gap = if desired_total + 3.0 * CONTROL_GAP <= available {
            CONTROL_GAP
        } else {
            0.0
        };
        let widths = if desired_total > available && desired_total > 0.0 {
            desired.map(|value| value * available / desired_total)
        } else {
            desired
        };
        let mut controls = [None; 4];
        let mut control_x = BAR_MARGIN + BAR_PADDING;
        let control_h = (bar_h - 2.0 * BAR_PADDING).max(0.0);
        for index in 0..controls.len() {
            let control_w = widths[index];
            controls[index] = nonzero_rect(physical_rect(
                control_x,
                BAR_MARGIN + (bar_h - control_h) / 2.0,
                control_w,
                control_h,
                scale,
                width,
                height,
            ));
            control_x += control_w + gap;
        }

        let controls_end = control_x - gap;
        let status_width = text_width("正在控制", font_scale) + 2.0 * BAR_PADDING;
        let status_x = controls_end + CONTROL_GAP;
        let mut summary_x = status_x;
        let status = if toolbar_visible && status_x + status_width <= BAR_MARGIN + bar_w {
            let rect = physical_rect(
                status_x,
                BAR_MARGIN + BAR_PADDING,
                status_width,
                control_h,
                scale,
                width,
                height,
            );
            summary_x = status_x + status_width + CONTROL_GAP;
            nonzero_rect(rect)
        } else {
            None
        };
        let summary = if toolbar_visible && summary_x + 144.0 <= BAR_MARGIN + bar_w {
            nonzero_rect(physical_rect(
                summary_x,
                BAR_MARGIN + BAR_PADDING,
                BAR_MARGIN + bar_w - summary_x,
                control_h,
                scale,
                width,
                height,
            ))
        } else {
            None
        };

        let stats_y = BAR_MARGIN + bar_h + BAR_MARGIN;
        let stats_w = (logical_w - 2.0 * BAR_MARGIN).clamp(0.0, 360.0);
        let stats_h = (logical_h - stats_y - BAR_MARGIN).clamp(0.0, 176.0);
        let stats = if toolbar_visible && stats_visible {
            nonzero_rect(physical_rect(
                logical_w - BAR_MARGIN - stats_w,
                stats_y,
                stats_w,
                stats_h,
                scale,
                width,
                height,
            ))
        } else {
            None
        };

        let paused_text = "控制已暂停 · Ctrl+Alt+Shift+Q";
        let paused_w = (text_width(paused_text, 1) + 2.0 * BAR_PADDING).min(logical_w);
        let paused_h = 24.0_f64.min(logical_h);
        let paused_banner = if paused && paused_w > 0.0 && paused_h > 0.0 {
            nonzero_rect(physical_rect(
                (logical_w - paused_w) / 2.0,
                logical_h - paused_h - BAR_MARGIN,
                paused_w,
                paused_h,
                scale,
                width,
                height,
            ))
        } else {
            None
        };

        Self {
            content,
            bar,
            controls,
            status,
            summary,
            stats,
            paused_banner,
            font_scale: scale_pixels(f64::from(font_scale), scale).max(1),
            stats_font_scale: scale_pixels(12.0, scale).max(1),
            compact,
            toolbar_visible,
        }
    }

    pub(crate) fn action_at(&self, x: f64, y: f64) -> Option<ToolbarAction> {
        if !self.toolbar_visible {
            return None;
        }
        ToolbarAction::ALL.into_iter().find(|action| {
            self.control(*action)
                .is_some_and(|rect| point_in_rect(x, y, rect))
        })
    }

    pub(crate) fn overlay_at(&self, x: f64, y: f64) -> bool {
        (self.toolbar_visible
            && (point_in_rect(x, y, self.bar)
                || self.status.is_some_and(|rect| point_in_rect(x, y, rect))
                || self.summary.is_some_and(|rect| point_in_rect(x, y, rect))
                || self.stats.is_some_and(|rect| point_in_rect(x, y, rect))))
            || self
                .paused_banner
                .is_some_and(|rect| point_in_rect(x, y, rect))
    }

    pub(crate) fn control(&self, action: ToolbarAction) -> Option<Rect> {
        let index = ToolbarAction::ALL
            .iter()
            .position(|candidate| *candidate == action)?;
        self.controls[index]
    }

    pub(crate) fn label(
        &self,
        action: ToolbarAction,
        fullscreen: bool,
        paused: bool,
    ) -> &'static str {
        action.label(fullscreen, paused, self.compact)
    }

    pub(crate) const fn content(&self) -> Option<Rect> {
        self.content
    }

    pub(crate) const fn bar(&self) -> Rect {
        self.bar
    }

    pub(crate) const fn status(&self) -> Option<Rect> {
        self.status
    }

    pub(crate) const fn summary(&self) -> Option<Rect> {
        self.summary
    }

    pub(crate) const fn stats(&self) -> Option<Rect> {
        self.stats
    }

    pub(crate) const fn paused_banner(&self) -> Option<Rect> {
        self.paused_banner
    }

    pub(crate) const fn font_scale(&self) -> u32 {
        self.font_scale
    }

    pub(crate) const fn stats_font_scale(&self) -> u32 {
        self.stats_font_scale
    }
}

pub(crate) fn content_rect(width: u32, height: u32) -> Option<Rect> {
    if width == 0 || height == 0 {
        None
    } else {
        Some(Rect {
            x: 0,
            y: 0,
            width,
            height,
        })
    }
}

pub(crate) fn map_cursor_to_remote(
    x: f64,
    y: f64,
    content: Rect,
    source: (u32, u32),
    gate: &InputGate,
    layout: &ToolbarLayout,
) -> Option<(u32, u32)> {
    if !gate.allows_remote_input() || layout.overlay_at(x, y) || !point_in_rect(x, y, content) {
        return None;
    }
    frame::cursor_to_frame(
        x - f64::from(content.x),
        y - f64::from(content.y),
        content.width,
        content.height,
        source.0,
        source.1,
    )
}

pub(crate) fn toolbar_height(logical_width: f64) -> u32 {
    if !logical_width.is_finite() || logical_width <= 720.0 {
        MIN_TOOLBAR_HEIGHT
    } else if logical_width <= 1100.0 {
        40
    } else if logical_width <= 1600.0 {
        48
    } else {
        MAX_TOOLBAR_HEIGHT
    }
}

fn valid_scale(scale: f64) -> f64 {
    if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    }
}

fn scale_pixels(logical: f64, scale: f64) -> u32 {
    (logical * valid_scale(scale))
        .round()
        .clamp(0.0, f64::from(u32::MAX)) as u32
}

fn physical_rect(
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    scale: f64,
    window_w: u32,
    window_h: u32,
) -> Rect {
    let px = scale_pixels(x.max(0.0), scale).min(window_w);
    let py = scale_pixels(y.max(0.0), scale).min(window_h);
    Rect {
        x: px,
        y: py,
        width: scale_pixels(width.max(0.0), scale).min(window_w.saturating_sub(px)),
        height: scale_pixels(height.max(0.0), scale).min(window_h.saturating_sub(py)),
    }
}

fn nonzero_rect(rect: Rect) -> Option<Rect> {
    if rect.width > 0 && rect.height > 0 {
        Some(rect)
    } else {
        None
    }
}

fn point_in_rect(x: f64, y: f64, rect: Rect) -> bool {
    x >= f64::from(rect.x)
        && y >= f64::from(rect.y)
        && x < f64::from(rect.x) + f64::from(rect.width)
        && y < f64::from(rect.y) + f64::from(rect.height)
}

#[cfg(feature = "window")]
fn text_width(text: &str, font_scale: u32) -> f64 {
    crate::overlay_text::text_width(text, font_scale)
}

#[cfg(not(feature = "window"))]
fn text_width(text: &str, font_scale: u32) -> f64 {
    text.chars()
        .map(|character| {
            let units = if ('\u{2e80}'..='\u{9fff}').contains(&character) {
                1.0
            } else {
                0.56
            };
            units * f64::from(font_scale)
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toolbar_height_stays_in_supported_logical_range() {
        for (width, expected) in [(640.0, 32), (1024.0, 40), (1440.0, 48), (1920.0, 56)] {
            let height = toolbar_height(width);
            assert!((MIN_TOOLBAR_HEIGHT..=MAX_TOOLBAR_HEIGHT).contains(&height));
            assert_eq!(height, expected);
        }
        assert_eq!(toolbar_height(0.0), MIN_TOOLBAR_HEIGHT);
        assert_eq!(toolbar_height(f64::NAN), MIN_TOOLBAR_HEIGHT);
    }

    #[test]
    fn minimum_client_fits_all_controls_at_every_dpi() {
        for scale in [1.0_f64, 1.25, 1.5, 2.0] {
            let width = (640.0 * scale).round() as u32;
            let height = (360.0 * scale).round() as u32;
            let layout = ToolbarLayout::new(width, height, scale, false, true, true);
            assert_eq!(layout.bar().height, (32.0 * scale).round() as u32);
            assert!(layout.content().is_some());
            for action in ToolbarAction::ALL {
                let rect = layout
                    .control(action)
                    .expect("minimum width keeps every control");
                assert!(rect.x >= layout.bar().x);
                assert!(rect.y >= layout.bar().y);
                assert!(rect.x + rect.width <= layout.bar().x + layout.bar().width);
                assert!(rect.y + rect.height <= layout.bar().y + layout.bar().height);
            }
            let stats = layout
                .stats()
                .expect("stats fit above the minimum client height");
            assert!(stats.y + stats.height <= height);
        }
    }

    #[test]
    fn tiny_windows_and_invalid_scale_never_panic_or_overflow() {
        for scale in [0.0, f64::NAN, f64::INFINITY, 1.25, 2.0] {
            for (width, height) in [(0, 0), (1, 1), (16, 16), (100, 20), (640, 1)] {
                let layout = ToolbarLayout::new(width, height, scale, true, true, true);
                assert_eq!(layout.content().is_some(), width > 0 && height > 0);
                assert!(layout.bar().x + layout.bar().width <= width);
                assert!(layout.bar().y + layout.bar().height <= height);
                assert!(layout.action_at(-1.0, -1.0).is_none());
            }
        }
    }

    #[test]
    fn hit_testing_uses_half_open_control_boundaries() {
        let layout = ToolbarLayout::new(640, 360, 1.0, false, false, true);
        let first = layout.control(ToolbarAction::Fullscreen).expect("control");
        let y = f64::from(first.y + first.height / 2);
        assert_eq!(
            layout.action_at(f64::from(first.x), y),
            Some(ToolbarAction::Fullscreen)
        );
        assert_eq!(
            layout.action_at(f64::from(first.x + first.width - 1), y),
            Some(ToolbarAction::Fullscreen)
        );
        assert_eq!(
            layout.action_at(f64::from(first.x + first.width) - 0.25, y),
            Some(ToolbarAction::Fullscreen)
        );
        assert_eq!(layout.action_at(f64::from(first.x + first.width), y), None);
        assert_eq!(layout.action_at(f64::NAN, y), None);
        assert_eq!(
            layout.action_at(f64::from(first.x), f64::from(first.y)),
            Some(ToolbarAction::Fullscreen)
        );
        assert_eq!(
            layout.action_at(
                f64::from(first.x + first.width / 2),
                f64::from(first.y + first.height)
            ),
            None
        );
    }

    #[test]
    fn auto_hide_waits_for_inactivity_but_not_hover_or_keyboard_focus() {
        let now = Instant::now();
        let mut state = ToolbarState::new(true);
        state.reveal(now);
        assert_eq!(state.next_wakeup(), Some(now + AUTO_HIDE_DELAY));
        assert!(!state.update(now + AUTO_HIDE_DELAY - Duration::from_millis(1)));
        state.set_hovered(true, now);
        assert_eq!(state.next_wakeup(), None);
        assert!(!state.update(now + AUTO_HIDE_DELAY * 3));
        state.set_hovered(false, now + AUTO_HIDE_DELAY * 3);
        state.set_focused(true, now + AUTO_HIDE_DELAY * 3);
        assert!(!state.update(now + AUTO_HIDE_DELAY * 6));
        state.set_focused(false, now + AUTO_HIDE_DELAY * 6);
        assert!(state.update(now + AUTO_HIDE_DELAY * 8));
        assert!(!state.visible());
        assert!(state.reveal(now + AUTO_HIDE_DELAY * 9));
        assert!(state.visible());

        let mut windowed = ToolbarState::new(false);
        windowed.reveal(now);
        assert!(!windowed.update(now + AUTO_HIDE_DELAY * 20));
        assert!(windowed.visible());
        assert_eq!(windowed.next_wakeup(), None);
    }

    #[test]
    fn pressed_control_delays_auto_hide() {
        let now = Instant::now();
        let mut state = ToolbarState::new(true);
        state.reveal(now);
        state.set_pressed(Some(ToolbarAction::Stats), now);
        assert!(!state.update(now + AUTO_HIDE_DELAY * 3));
        state.set_pressed(None, now + AUTO_HIDE_DELAY * 3);
        assert!(state.update(now + AUTO_HIDE_DELAY * 5));
    }

    #[test]
    fn input_gate_blocks_paused_and_overlay_then_restores_video() {
        let mut gate = InputGate::default();
        assert!(gate.allows_remote_input());
        gate.set_overlay(true);
        assert!(!gate.allows_remote_input());
        gate.set_overlay(false);
        assert!(gate.allows_remote_input());
        assert!(gate.toggle_paused());
        gate.set_overlay(false);
        assert!(!gate.allows_remote_input());
        assert!(!gate.toggle_paused());
        assert!(gate.allows_remote_input());
    }

    #[test]
    fn overlay_cursor_mapping_resumes_immediately_in_video() {
        for scale in [1.25_f64, 1.5, 2.0] {
            let width = (640.0 * scale).round() as u32;
            let height = (360.0 * scale).round() as u32;
            let layout = ToolbarLayout::new(width, height, scale, false, true, true);
            let content = layout.content().expect("content");
            let control = layout.control(ToolbarAction::Fullscreen).expect("control");
            let overlay_point = (
                f64::from(control.x + control.width / 2),
                f64::from(control.y + control.height / 2),
            );
            let mut gate = InputGate::default();
            gate.set_overlay(true);
            assert_eq!(
                map_cursor_to_remote(
                    overlay_point.0,
                    overlay_point.1,
                    content,
                    (1280, 720),
                    &gate,
                    &layout
                ),
                None
            );
            gate.set_overlay(false);
            assert_eq!(
                map_cursor_to_remote(
                    overlay_point.0,
                    overlay_point.1,
                    content,
                    (1280, 720),
                    &gate,
                    &layout
                ),
                None
            );
            let mapped = map_cursor_to_remote(
                f64::from(width / 4),
                f64::from(height * 3 / 4),
                content,
                (1280, 720),
                &gate,
                &layout,
            )
            .expect("video point remains interactive");
            assert!((i64::from(mapped.0) - 320).abs() <= 1);
            assert!((539..=540).contains(&mapped.1));
            assert_eq!(
                map_cursor_to_remote(
                    f64::from(width / 2),
                    f64::from(height / 2),
                    content,
                    (1280, 720),
                    &InputGate {
                        paused: true,
                        ..InputGate::default()
                    },
                    &layout
                ),
                None
            );
        }
    }

    #[test]
    fn content_rectangle_rejects_only_empty_clients() {
        assert_eq!(content_rect(0, 360), None);
        assert_eq!(content_rect(640, 0), None);
        assert_eq!(
            content_rect(1, 1),
            Some(Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1
            })
        );
    }
}
