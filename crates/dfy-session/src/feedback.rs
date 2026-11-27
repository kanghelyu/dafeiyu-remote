// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

use std::time::{Duration, Instant};

use dfy_proto::StreamFeedback;

pub const FEEDBACK_INTERVAL: Duration = Duration::from_millis(500);
const MAX_FEEDBACK_WINDOW_MS: u64 = 60_000;
const ACTIVE_GAP_LIMIT: Duration = Duration::from_secs(3);

#[derive(Debug, Default)]
pub struct StreamFeedbackWindow {
    started_at: Option<Instant>,
    last_packet_at: Option<Instant>,
    payload_bytes: u64,
    packets: u64,
    frames_assembled: u64,
    frames_delivered: u64,
    dropped_frames: u64,
    stall_ms: u64,
}

impl StreamFeedbackWindow {
    pub fn new(started_at: Instant) -> Self {
        Self {
            started_at: Some(started_at),
            ..Self::default()
        }
    }

    pub fn note_packet(&mut self, now: Instant, payload_bytes: usize) {
        if self.packets == 0 && self.dropped_frames == 0 {
            self.started_at = Some(now);
        }
        self.started_at.get_or_insert(now);
        if let Some(last) = self.last_packet_at {
            let gap = now.saturating_duration_since(last);
            if gap > ACTIVE_GAP_LIMIT {
                self.last_packet_at = None;
                self.stall_ms = 0;
            } else {
                self.stall_ms = self.stall_ms.max(gap.as_millis() as u64);
            }
        }
        self.last_packet_at = Some(now);
        self.payload_bytes = self.payload_bytes.saturating_add(payload_bytes as u64);
        self.packets = self.packets.saturating_add(1);
    }

    pub fn note_assembled(&mut self) {
        self.frames_assembled = self.frames_assembled.saturating_add(1);
    }

    pub fn note_delivered(&mut self) {
        self.frames_delivered = self.frames_delivered.saturating_add(1);
    }

    pub fn set_dropped_frames(&mut self, dropped_frames: u64) {
        self.dropped_frames = dropped_frames;
    }

    pub fn take(&mut self, now: Instant) -> Option<StreamFeedback> {
        let started_at = self.started_at?;
        let window_ms = now.saturating_duration_since(started_at).as_millis() as u64;
        let feedback = (self.packets > 0 || self.dropped_frames > 0).then(|| StreamFeedback {
            window_ms: window_ms.clamp(1, MAX_FEEDBACK_WINDOW_MS),
            payload_bytes: self.payload_bytes,
            packets: self.packets,
            frames_assembled: self.frames_assembled,
            frames_delivered: self.frames_delivered,
            dropped_frames: self.dropped_frames,
            stall_ms: self.stall_ms.min(MAX_FEEDBACK_WINDOW_MS),
        });
        let last_packet_at = self
            .last_packet_at
            .filter(|last| now.saturating_duration_since(*last) <= ACTIVE_GAP_LIMIT);
        *self = Self {
            started_at: Some(now),
            last_packet_at,
            ..Self::default()
        };
        feedback
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_idle_desktop_and_resets_after_take() {
        let start = Instant::now();
        let mut window = StreamFeedbackWindow::new(start);
        assert!(window.take(start + FEEDBACK_INTERVAL).is_none());
        window.note_packet(start + Duration::from_secs(10), 100);
        let feedback = window
            .take(start + Duration::from_secs(10) + FEEDBACK_INTERVAL)
            .unwrap();
        assert_eq!(feedback.window_ms, 500);
        assert_eq!(feedback.stall_ms, 0);
        assert_eq!(feedback.payload_bytes, 100);
        assert!(window.take(start + Duration::from_secs(11)).is_none());
    }

    #[test]
    fn measures_active_gaps_and_frame_outcomes() {
        let start = Instant::now();
        let mut window = StreamFeedbackWindow::new(start);
        window.note_packet(start, 100);
        window.note_packet(start + Duration::from_millis(750), 100);
        window.note_assembled();
        window.note_assembled();
        window.note_delivered();
        window.set_dropped_frames(1);
        let feedback = window.take(start + Duration::from_millis(1_000)).unwrap();
        assert_eq!(feedback.window_ms, 1_000);
        assert_eq!(feedback.stall_ms, 750);
        assert_eq!(feedback.packets, 2);
        assert_eq!(feedback.frames_assembled, 2);
        assert_eq!(feedback.frames_delivered, 1);
        assert_eq!(feedback.dropped_frames, 1);
    }

    #[test]
    fn measures_active_gap_across_feedback_windows() {
        let start = Instant::now();
        let mut window = StreamFeedbackWindow::new(start);
        window.note_packet(start, 100);
        window.take(start + FEEDBACK_INTERVAL).unwrap();
        window.note_packet(start + Duration::from_millis(2_400), 100);
        let feedback = window.take(start + Duration::from_millis(2_900)).unwrap();
        assert_eq!(feedback.stall_ms, 2_400);
    }

    #[test]
    fn idle_gap_after_take_is_not_a_stall() {
        let start = Instant::now();
        let mut window = StreamFeedbackWindow::new(start);
        window.note_packet(start, 100);
        window.take(start + FEEDBACK_INTERVAL).unwrap();
        window.note_packet(start + Duration::from_secs(10), 100);
        let feedback = window
            .take(start + Duration::from_secs(10) + FEEDBACK_INTERVAL)
            .unwrap();
        assert_eq!(feedback.stall_ms, 0);
    }

    #[test]
    fn reports_drops_without_packets() {
        let start = Instant::now();
        let mut window = StreamFeedbackWindow::new(start);
        window.set_dropped_frames(3);
        let feedback = window.take(start + FEEDBACK_INTERVAL).unwrap();
        assert_eq!(feedback.packets, 0);
        assert_eq!(feedback.dropped_frames, 3);
        assert_eq!(feedback.payload_bytes, 0);
    }
}
