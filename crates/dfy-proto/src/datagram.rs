//! DATAGRAM 头编解码（不可靠、可丢）：
//!
//! - 视频（`chan = 1`）：`u8 chan | u8 flags | u32 frame_id | u16 frame_seq | u16 pkt_idx | u16 pkt_cnt | u32 ts_us | payload`
//! - 输入位移（`chan = 0`）：`u8 chan | InputEvent protobuf`
//!
//! 数值一律**大端**（与控制帧长度前缀一致）。

use prost::Message as _;

use crate::{
    consts::{CHAN_INPUT_MOVE, CHAN_VIDEO, VIDEO_FLAG_KEY, VIDEO_FLAG_LAST, VIDEO_HEADER_LEN},
    msg::InputEvent,
    ProtoError,
};

/// 视频 DATAGRAM 头（16 字节，不含负载）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoHeader {
    /// 通道号，固定 [`CHAN_VIDEO`]。
    pub chan: u8,
    /// flags：KEY / LAST / FEC / XOR_GROUP，见 [`VIDEO_FLAG_KEY`] 等。
    pub flags: u8,
    /// 帧 ID（单调递增）。
    pub frame_id: u32,
    /// 帧内/流内顺序号（用于丢包与重排判断）。
    pub frame_seq: u16,
    /// 本分片序号（0 起）。
    pub pkt_idx: u16,
    /// 本帧分片总数（必须 ≥ 1）。
    pub pkt_cnt: u16,
    /// 采集时间戳（Unix 微秒，截断到 u32）。
    pub ts_us: u32,
}

impl VideoHeader {
    /// 构造一个单包帧头（`pkt_idx = 0, pkt_cnt = 1`）。
    pub fn single(frame_id: u32, frame_seq: u16, flags: u8, ts_us: u32) -> Self {
        Self {
            chan: CHAN_VIDEO,
            flags,
            frame_id,
            frame_seq,
            pkt_idx: 0,
            pkt_cnt: 1,
            ts_us,
        }
    }

    /// 是否关键帧。
    pub fn is_key(&self) -> bool {
        self.flags & VIDEO_FLAG_KEY != 0
    }

    /// 是否本帧最后一个分片。
    pub fn is_last(&self) -> bool {
        self.flags & VIDEO_FLAG_LAST != 0
    }

    /// 编码 16B 头到 `out`（不会越界写）；`out` 长度必须 ≥ [`VIDEO_HEADER_LEN`]。
    pub fn encode(&self, out: &mut [u8]) -> Result<(), ProtoError> {
        if out.len() < VIDEO_HEADER_LEN {
            return Err(ProtoError::ShortHeader {
                need: VIDEO_HEADER_LEN,
                got: out.len(),
            });
        }
        if self.chan != CHAN_VIDEO {
            return Err(ProtoError::BadChannel { got: self.chan });
        }
        if self.pkt_cnt == 0 || self.pkt_idx >= self.pkt_cnt {
            return Err(ProtoError::BadPacketIndex {
                idx: self.pkt_idx,
                cnt: self.pkt_cnt,
            });
        }
        out[0] = self.chan;
        out[1] = self.flags;
        out[2..6].copy_from_slice(&self.frame_id.to_be_bytes());
        out[6..8].copy_from_slice(&self.frame_seq.to_be_bytes());
        out[8..10].copy_from_slice(&self.pkt_idx.to_be_bytes());
        out[10..12].copy_from_slice(&self.pkt_cnt.to_be_bytes());
        out[12..16].copy_from_slice(&self.ts_us.to_be_bytes());
        Ok(())
    }

    /// 解码 16B 头。
    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        if buf.len() < VIDEO_HEADER_LEN {
            return Err(ProtoError::ShortHeader {
                need: VIDEO_HEADER_LEN,
                got: buf.len(),
            });
        }
        let chan = buf[0];
        if chan != CHAN_VIDEO {
            return Err(ProtoError::BadChannel { got: chan });
        }
        let hdr = Self {
            chan,
            flags: buf[1],
            frame_id: u32::from_be_bytes([buf[2], buf[3], buf[4], buf[5]]),
            frame_seq: u16::from_be_bytes([buf[6], buf[7]]),
            pkt_idx: u16::from_be_bytes([buf[8], buf[9]]),
            pkt_cnt: u16::from_be_bytes([buf[10], buf[11]]),
            ts_us: u32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]),
        };
        if hdr.pkt_cnt == 0 || hdr.pkt_idx >= hdr.pkt_cnt {
            return Err(ProtoError::BadPacketIndex {
                idx: hdr.pkt_idx,
                cnt: hdr.pkt_cnt,
            });
        }
        Ok(hdr)
    }
}

/// 编码一个完整视频 DATAGRAM（头 + 负载）。
pub fn encode_video(header: &VideoHeader, payload: &[u8]) -> Result<Vec<u8>, ProtoError> {
    let mut out = vec![0u8; VIDEO_HEADER_LEN + payload.len()];
    header.encode(&mut out[..VIDEO_HEADER_LEN])?;
    out[VIDEO_HEADER_LEN..].copy_from_slice(payload);
    Ok(out)
}

/// 解码视频 DATAGRAM：返回 (头, 负载切片)。
pub fn decode_video(buf: &[u8]) -> Result<(VideoHeader, &[u8]), ProtoError> {
    let header = VideoHeader::decode(buf)?;
    Ok((header, &buf[VIDEO_HEADER_LEN..]))
}

/// 编码输入位移 DATAGRAM：`chan(0) | InputEvent`。
pub fn encode_input_move(event: &InputEvent) -> Result<Vec<u8>, ProtoError> {
    let body = event.encode_to_vec();
    let mut out = Vec::with_capacity(1 + body.len());
    out.push(CHAN_INPUT_MOVE);
    out.extend_from_slice(&body);
    Ok(out)
}

/// 解码输入位移 DATAGRAM。
pub fn decode_input_move(buf: &[u8]) -> Result<InputEvent, ProtoError> {
    match buf.first() {
        Some(&CHAN_INPUT_MOVE) => Ok(InputEvent::decode(&buf[1..])?),
        Some(&got) => Err(ProtoError::BadChannel { got }),
        None => Err(ProtoError::ShortHeader { need: 1, got: 0 }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consts::{INPUT_KIND_MOUSE_MOVE, INPUT_STATE_DOWN, VIDEO_FLAG_FEC};

    fn sample_event() -> InputEvent {
        InputEvent {
            kind: INPUT_KIND_MOUSE_MOVE,
            hid_usage: 0,
            mods: 0,
            state: INPUT_STATE_DOWN,
            x: 1234,
            y: 5678,
            dx: -3,
            dy: 9,
            unicode: 0,
            ts_us: 0x0102_0304_0506_0708,
        }
    }

    #[test]
    fn video_header_field_layout() {
        let h = VideoHeader::single(
            0x0102_0304,
            0x0506,
            VIDEO_FLAG_KEY | VIDEO_FLAG_LAST,
            0x0A0B_0C0D,
        );
        let mut buf = [0u8; VIDEO_HEADER_LEN];
        h.encode(&mut buf).unwrap();
        assert_eq!(buf[0], 1, "chan byte");
        assert_eq!(buf[1], 0b11, "flags byte");
        assert_eq!(&buf[2..6], &[1, 2, 3, 4], "frame_id 大端");
        assert_eq!(&buf[6..8], &[5, 6], "frame_seq 大端");
        assert_eq!(&buf[8..10], &[0, 0], "pkt_idx 大端");
        assert_eq!(&buf[10..12], &[0, 1], "pkt_cnt 大端");
        assert_eq!(&buf[12..16], &[0x0A, 0x0B, 0x0C, 0x0D], "ts_us 大端");
    }

    #[test]
    fn video_header_roundtrip_and_flags() {
        let h = VideoHeader {
            chan: CHAN_VIDEO,
            flags: VIDEO_FLAG_KEY | VIDEO_FLAG_FEC,
            frame_id: 41,
            frame_seq: 3,
            pkt_idx: 2,
            pkt_cnt: 5,
            ts_us: 999,
        };
        let mut buf = [0u8; VIDEO_HEADER_LEN];
        h.encode(&mut buf).unwrap();
        let got = VideoHeader::decode(&buf).unwrap();
        assert_eq!(got, h);
        assert!(got.is_key());
        assert!(!got.is_last());
    }

    #[test]
    fn video_header_rejects_bad_channel() {
        let mut buf = [0u8; VIDEO_HEADER_LEN];
        buf[0] = CHAN_INPUT_MOVE;
        assert!(matches!(
            VideoHeader::decode(&buf),
            Err(ProtoError::BadChannel { got: 0 })
        ));
        let h = VideoHeader::single(1, 0, 0, 0);
        let mut bad = h;
        bad.chan = 7;
        let mut out = [0u8; VIDEO_HEADER_LEN];
        assert!(matches!(
            bad.encode(&mut out),
            Err(ProtoError::BadChannel { got: 7 })
        ));
    }

    #[test]
    fn video_header_rejects_bad_packet_index() {
        let mut h = VideoHeader::single(1, 0, 0, 0);
        h.pkt_cnt = 0;
        let mut out = [0u8; VIDEO_HEADER_LEN];
        assert!(matches!(
            h.encode(&mut out),
            Err(ProtoError::BadPacketIndex { .. })
        ));

        let mut buf = [0u8; VIDEO_HEADER_LEN];
        VideoHeader::single(1, 0, 0, 0).encode(&mut buf).unwrap();
        buf[8..10].copy_from_slice(&5u16.to_be_bytes());
        assert!(matches!(
            VideoHeader::decode(&buf),
            Err(ProtoError::BadPacketIndex { idx: 5, cnt: 1 })
        ));
    }

    #[test]
    fn video_header_rejects_short_buffer() {
        let buf = [0u8; VIDEO_HEADER_LEN - 1];
        assert!(matches!(
            VideoHeader::decode(&buf),
            Err(ProtoError::ShortHeader {
                need: VIDEO_HEADER_LEN,
                got: 15
            })
        ));
        assert!(matches!(
            VideoHeader::single(1, 0, 0, 0).encode(&mut [0u8; 4]),
            Err(ProtoError::ShortHeader { need: 16, got: 4 })
        ));
    }

    #[test]
    fn video_datagram_roundtrip_with_payload() {
        let payload = vec![0x5Au8; 1200];
        let h = VideoHeader::single(7, 7, VIDEO_FLAG_KEY | VIDEO_FLAG_LAST, 123_456);
        let dgram = encode_video(&h, &payload).unwrap();
        assert_eq!(dgram.len(), VIDEO_HEADER_LEN + 1200);
        let (got_h, got_payload) = decode_video(&dgram).unwrap();
        assert_eq!(got_h, h);
        assert_eq!(got_payload, payload.as_slice());
    }

    #[test]
    fn input_move_roundtrip() {
        let ev = sample_event();
        let dgram = encode_input_move(&ev).unwrap();
        assert_eq!(dgram[0], CHAN_INPUT_MOVE);
        let got = decode_input_move(&dgram).unwrap();
        assert_eq!(got, ev);
        assert_eq!(got.dx, -3);
        assert_eq!(got.dy, 9);
    }

    #[test]
    fn input_move_rejects_other_channel_and_empty() {
        let mut dgram = encode_input_move(&sample_event()).unwrap();
        dgram[0] = CHAN_VIDEO;
        assert!(matches!(
            decode_input_move(&dgram),
            Err(ProtoError::BadChannel { got: 1 })
        ));
        assert!(matches!(
            decode_input_move(&[]),
            Err(ProtoError::ShortHeader { need: 1, got: 0 })
        ));
    }
}
