//! 不可靠 DATAGRAM 通道：视频分片发送 / input-move 发送 / 统计接收。
//!
//! - `video` 通道（`CHAN_VIDEO = 1`）的负载按 ≤1100B 分片，每片带 16B 头
//!   （`frame_id/frame_seq/pkt_idx/pkt_cnt/ts_us/flags`）。QUIC 路径 MTU 默认 1200，
//!   因此 16 + 1100 = 1116B 可单包通过；若对端协商到更小 DATAGRAM 上限则自动再切。
//! - `input-move` 通道（`CHAN_INPUT_MOVE = 0`）直接放一个 `InputEvent` protobuf。

use std::collections::HashSet;

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use dfy_proto::{
    decode_video, encode_input_move, encode_video, InputEvent, VideoHeader, CHAN_VIDEO,
    VIDEO_FLAG_KEY, VIDEO_FLAG_LAST, VIDEO_HEADER_LEN, VIDEO_MAX_PAYLOAD,
};
use quinn::Connection;

/// 一帧视频分片发送结果。
#[derive(Clone, Copy, Debug, Default)]
pub struct VideoSendStats {
    /// 发出的 DATAGRAM 包数。
    pub packets: usize,
    /// 发出的视频负载总字节。
    pub bytes: usize,
}

/// 发送一帧视频：按对端可接受的最大 DATAGRAM 大小自动分片。
///
/// `frame_id` 单调递增，`frame_seq` 用于重排/丢帧检测；`keyframe` 时首片打 `KEY` 标记，
/// 末片打 `LAST` 标记。
pub fn send_video_frame(
    conn: &Connection,
    frame_id: u32,
    frame_seq: u16,
    ts_us: u32,
    payload: &[u8],
    keyframe: bool,
) -> Result<VideoSendStats> {
    let max_datagram = conn
        .max_datagram_size()
        .ok_or_else(|| anyhow!("对端未协商 DATAGRAM 扩展（max_datagram_size = None）"))?;
    anyhow::ensure!(
        max_datagram > VIDEO_HEADER_LEN + 1,
        "对端 DATAGRAM 上限 {max_datagram}B 太小，装不下 {VIDEO_HEADER_LEN}B 头 + 负载"
    );

    let chunk = (max_datagram - VIDEO_HEADER_LEN)
        .min(VIDEO_MAX_PAYLOAD)
        .max(1);
    let packet_count = if payload.is_empty() {
        1
    } else {
        payload.len().div_ceil(chunk)
    };
    anyhow::ensure!(
        packet_count <= u16::MAX as usize,
        "单帧 {}B 需要 {packet_count} 个分片，超过 pkt_cnt 上限 u16",
        payload.len()
    );

    let mut stats = VideoSendStats::default();
    for idx in 0..packet_count {
        let start = (idx * chunk).min(payload.len());
        let end = (start + chunk).min(payload.len());
        let mut flags = 0u8;
        if keyframe && idx == 0 {
            flags |= VIDEO_FLAG_KEY;
        }
        if idx + 1 == packet_count {
            flags |= VIDEO_FLAG_LAST;
        }
        let header = VideoHeader {
            chan: CHAN_VIDEO,
            flags,
            frame_id,
            frame_seq,
            pkt_idx: idx as u16,
            pkt_cnt: packet_count as u16,
            ts_us,
        };
        let datagram = encode_video(&header, &payload[start..end])
            .with_context(|| format!("编码视频分片失败（frame_id={frame_id}, pkt_idx={idx}）"))?;
        stats.bytes += end - start;
        stats.packets += 1;
        conn.send_datagram(Bytes::from(datagram)).map_err(|e| {
            anyhow!(
                "发送视频 DATAGRAM 失败（frame_id={frame_id}, pkt_idx={idx}, {}B）: {e}",
                end - start
            )
        })?;
    }
    Ok(stats)
}

/// 发送一个 input-move DATAGRAM（鼠标移动 / 触摸拖动等高频事件）。
pub fn send_input_move(conn: &Connection, event: &InputEvent) -> Result<()> {
    let datagram = encode_input_move(event).context("编码 input-move DATAGRAM 失败")?;
    conn.send_datagram(Bytes::from(datagram))
        .map_err(|e| anyhow!("发送 input-move DATAGRAM 失败: {e}"))
}

/// 视频 DATAGRAM 接收端统计（帧数/包数/字节/关键帧/乱序与重复包）。
#[derive(Default)]
pub struct VideoReceiver {
    packets: u64,
    bytes: u64,
    key_packets: u64,
    last_packets: u64,
    frames: HashSet<u32>,
    malformed: u64,
    out_of_order: u64,
}

impl VideoReceiver {
    /// 新建统计器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 处理一个收到的 DATAGRAM（非视频通道/坏包只计数，不中断会话）。
    pub fn ingest(&mut self, datagram: &[u8]) -> Result<VideoHeader> {
        let (header, payload) = decode_video(datagram).map_err(|e| {
            self.malformed += 1;
            anyhow!("解析视频 DATAGRAM 失败: {e}")
        })?;
        if header.pkt_idx == 0 {
            self.frames.insert(header.frame_id);
        } else if self.frames.insert(header.frame_id) {
            self.out_of_order += 1;
        }
        self.packets += 1;
        self.bytes += payload.len() as u64;
        if header.is_key() {
            self.key_packets += 1;
        }
        if header.is_last() {
            self.last_packets += 1;
        }
        Ok(header)
    }

    /// 已统计到的不同帧数。
    pub fn frames(&self) -> usize {
        self.frames.len()
    }

    /// 收到的 DATAGRAM 包数。
    pub fn packets(&self) -> u64 {
        self.packets
    }

    /// 收到的视频负载字节数。
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// 首片带 `KEY` 标志的包数。
    pub fn key_packets(&self) -> u64 {
        self.key_packets
    }

    /// 带 `LAST` 标志的包数（理论等于完整帧数）。
    pub fn last_packets(&self) -> u64 {
        self.last_packets
    }

    /// 解析失败的 DATAGRAM 数。
    pub fn malformed(&self) -> u64 {
        self.malformed
    }

    /// 非首片先到的乱序计数。
    pub fn out_of_order(&self) -> u64 {
        self.out_of_order
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dfy_proto::{decode_input_move, encode_input_move as encode_in, INPUT_KIND_MOUSE_MOVE};

    #[test]
    fn video_receiver_counts_frames_and_bytes() {
        let mut rx = VideoReceiver::new();
        let payload = vec![7u8; 300];
        for frame_id in 0..3u32 {
            for idx in 0..2u16 {
                let header = VideoHeader {
                    chan: CHAN_VIDEO,
                    flags: (if idx == 0 { VIDEO_FLAG_KEY } else { 0 })
                        | (if idx == 1 { VIDEO_FLAG_LAST } else { 0 }),
                    frame_id,
                    frame_seq: frame_id as u16,
                    pkt_idx: idx,
                    pkt_cnt: 2,
                    ts_us: 1_000 * frame_id,
                };
                let dg = encode_video(&header, &payload).expect("编码视频");
                rx.ingest(&dg).expect("统计视频");
            }
        }
        assert_eq!(rx.frames(), 3);
        assert_eq!(rx.packets(), 6);
        assert_eq!(rx.bytes(), 6 * 300);
        assert_eq!(rx.key_packets(), 3);
        assert_eq!(rx.last_packets(), 3);
        assert_eq!(rx.malformed(), 0);
    }

    #[test]
    fn malformed_video_datagram_is_counted_not_fatal() {
        let mut rx = VideoReceiver::new();
        assert!(rx.ingest(b"\x01\x00").is_err());
        assert_eq!(rx.malformed(), 1);
        assert_eq!(rx.packets(), 0);
    }

    #[test]
    fn input_move_datagram_roundtrip() {
        let event = InputEvent {
            kind: INPUT_KIND_MOUSE_MOVE,
            hid_usage: 0,
            mods: 0,
            state: 0,
            x: 100,
            y: 200,
            dx: -3,
            dy: 5,
            unicode: 0,
            ts_us: 42,
        };
        let dg = encode_in(&event).expect("编码 input-move");
        let back = decode_input_move(&dg).expect("解码 input-move");
        assert_eq!(back, event);
    }
}
