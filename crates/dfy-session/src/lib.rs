// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! # dfy-session — 会话编排（大肥鱼远程）
//!
//! 把「采集 → 编码 → QUIC DATAGRAM 发送」与「DATAGRAM 接收 → 帧重组 → 解码」串成可运行的
//! 两端会话，并处理 control 流的心跳、Bye 与自定义消息。
//!
//! 设计约束：
//! - **采集与编码跑在独立 OS 线程**（DXGI 采集对象绑定创建线程且不是 `Send`，编码器同理）；
//!   异步侧只负责发 DATAGRAM，避免阻塞 tokio 工作线程。
//! - 采集线程通过有界 `tokio::sync::mpsc` 输出 [`EncodedFrame`]，队列满即丢帧（实时优先）。
//! - 视频 DATAGRAM 在接收侧重组成整帧（[`FrameAssembler`]），按 `pkt_cnt` 校验完整性。
//! - 编码器/解码器以 trait 抽象（[`Encoder`]/[`Decoder`]），`dfy-codec` 提供真实实现，
//!   测试与 selftest 用 [`fake`] 里的替身。
//!
//! ```no_run
//! # async fn demo() -> anyhow::Result<()> {
//! use dfy_session::{serve_agent, AgentConfig, CaptureLoopConfig, start_capture_loop, FakeCapturer, FakeEncoder};
//! # let (server_ep, _identity, addr) = dfy_transport::bind_agent_random()?;
//! # let connecting = server_ep.accept().await.unwrap();
//! # let conn = connecting.await?;
//! let (welcome, chan) = dfy_transport::server_handshake(&conn, &dfy_transport::ServerAuth::new("pw", "agent")).await?;
//! let mut loop_ = start_capture_loop(CaptureLoopConfig::default(), || {
//!     Ok((FakeCapturer::new(320, 240, u64::MAX), FakeEncoder::new()))
//! })?;
//! let stats = serve_agent(conn, welcome, chan, &mut loop_, dfy_session::IgnoreHandler, AgentConfig::default()).await?;
//! println!("sent {} frames", stats.frames_sent);
//! # Ok(()) }
//! ```

#![forbid(unsafe_code)]

use std::collections::HashMap;

use dfy_proto::{VideoHeader, CHAN_VIDEO, VIDEO_FLAG_KEY, VIDEO_FLAG_LAST};

pub mod adaptation;
pub mod agent;
pub mod capture_loop;
pub mod client;
pub mod fake;
mod feedback;
pub mod migration;

pub use adaptation::StreamAdaptationConfig;
pub use agent::{
    serve_agent, serve_agent_shared, AgentConfig, AgentRunStats, CtrlHandler, IgnoreHandler,
};
pub use capture_loop::{
    start_capture_loop, start_capture_loop_with_encoder_factory, CaptureHandle, CaptureLoop,
    CaptureLoopConfig, CaptureRuntimeControl, CaptureStats, EncodeResolution, FrameInput,
    RuntimeSettings,
};
pub use client::{run_client, ClientConfig, ClientRunStats};
pub use fake::{FakeCapturer, FakeDecoder, FakeEncoder};
pub use feedback::{StreamFeedbackWindow, FEEDBACK_INTERVAL};
pub use migration::{MigrationContext, SessionCoordinator, SessionLease};

/// 采集线程输出的一帧原始图像（与 `dfy-capture` 解耦，便于测试替身）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawFrame {
    /// 宽（像素）。
    pub width: u32,
    /// 高（像素）。
    pub height: u32,
    /// 行跨距（字节，可能含 padding）。
    pub stride: u32,
    /// BGRA8 像素数据（`stride * height` 字节）。
    pub data: Vec<u8>,
    /// 采集时间戳（Unix 微秒）。
    pub pts_us: u64,
    /// 是否整帧（false 表示只有脏区更新，MVP 仍按整帧编码）。
    pub full_frame: bool,
}

/// 编码器输出的一份码流（H.264 Annex-B 为 MVP 约定；其它编码由实现方定义）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedFrame {
    /// 码流字节。
    pub data: Vec<u8>,
    /// 是否关键帧（IDR / SPS+PPS+IDR）。
    pub keyframe: bool,
    /// 采集时间戳（Unix 微秒）。
    pub pts_us: u64,
}

/// 解码器输出的一帧原始图像（MVP 约定 BGRA8；`stride` 可能大于 `width * 4`）。
#[derive(Clone, Debug)]
pub struct DecodedFrame {
    /// 宽（像素）。
    pub width: u32,
    /// 高（像素）。
    pub height: u32,
    /// 行跨距（字节）。
    pub stride: u32,
    /// BGRA8 像素数据。
    pub data: Vec<u8>,
    /// 采集时间戳（Unix 微秒）。
    pub pts_us: u64,
}

/// 屏幕采集源（Windows 上是 DXGI Desktop Duplication）。
pub trait Capturer {
    /// 采集一帧；`Ok(None)` 表示本次超时无新帧（正常现象），可继续调用。
    fn capture(&mut self, timeout_ms: u32) -> anyhow::Result<Option<RawFrame>>;

    /// 当前采集表面尺寸；未知时返回 `None`。
    ///
    /// 运行时分辨率控制用它提前校验/缩放，无需等待第一帧。默认实现保持旧采集器兼容。
    fn dimensions(&self) -> Option<(u32, u32)> {
        None
    }

    /// 取出一次采集源恢复通知；采集循环据此请求恢复后的首帧为关键帧。
    fn take_recovery_request(&mut self) -> bool {
        false
    }
}

/// 视频编码器。
pub trait Encoder {
    /// 编码一帧；返回 0..n 份码流（如 H.264 的 IDR + 后续非 IDR 可能分多次返回）。
    fn encode(&mut self, frame: &RawFrame) -> anyhow::Result<Vec<EncodedFrame>>;
    /// 请求下一帧强制关键帧（例如客户端新连接/丢帧后）。
    fn request_keyframe(&mut self);
    /// 运行时调整码率（kbps）；不支持时必须返回明确错误，不得静默接受。
    fn set_bitrate(&mut self, _kbps: u32) -> anyhow::Result<()> {
        anyhow::bail!("dfy-session: 当前编码器不支持动态码率调整")
    }
    /// 运行时调整帧率（fps）；不支持时必须返回明确错误，不得静默接受。
    fn set_frame_rate(&mut self, _fps: u32) -> anyhow::Result<()> {
        anyhow::bail!("dfy-session: 当前编码器不支持动态帧率调整；请通过编码器工厂重建")
    }
    /// 返回当前编码器设置（若实现能提供）。采集循环不会凭空假设原生码率。
    fn runtime_settings(&self) -> Option<crate::capture_loop::RuntimeSettings> {
        None
    }
    /// 冲刷内部缓冲。分辨率切换时调用。
    ///
    /// 无缓冲实现必须显式返回空列表；默认返回错误，避免在未实现冲刷时
    /// 丢弃旧编码器待输出帧并宣称可以安全重建。
    fn flush(&mut self) -> anyhow::Result<Vec<EncodedFrame>> {
        anyhow::bail!("dfy-session: 当前编码器未实现 flush，不能安全重建编码器")
    }
}

/// 视频解码器。
pub trait Decoder: Send {
    /// 解码一份码流；返回解出的帧（B 帧/需要更多数据时可为 `None`）。
    fn decode(&mut self, data: &[u8], pts_us: u64) -> anyhow::Result<Option<DecodedFrame>>;
}

#[cfg(windows)]
impl Capturer for dfy_capture::WindowsCapturer {
    fn capture(&mut self, timeout_ms: u32) -> anyhow::Result<Option<RawFrame>> {
        let frame = dfy_capture::WindowsCapturer::capture(self, timeout_ms)?;
        Ok(frame.map(|f| RawFrame {
            width: f.width,
            height: f.height,
            stride: f.stride,
            data: f.data,
            pts_us: dfy_transport::now_us(),
            full_frame: f.full_frame,
        }))
    }

    fn dimensions(&self) -> Option<(u32, u32)> {
        let monitor = dfy_capture::WindowsCapturer::monitor(self);
        Some((monitor.w, monitor.h))
    }
}

#[cfg(target_os = "linux")]
impl Capturer for dfy_capture::LinuxCapturer {
    fn capture(&mut self, _timeout_ms: u32) -> anyhow::Result<Option<RawFrame>> {
        // X11 GetImage 每次立即返回一帧（无阻塞能力），节流由采集线程负责。
        let frame = dfy_capture::LinuxCapturer::capture(self, 0)?;
        Ok(frame.map(|f| RawFrame {
            width: f.width,
            height: f.height,
            stride: f.stride,
            data: f.data,
            pts_us: dfy_transport::now_us(),
            full_frame: f.full_frame,
        }))
    }

    fn dimensions(&self) -> Option<(u32, u32)> {
        let monitor = dfy_capture::LinuxCapturer::monitor(self);
        Some((monitor.w, monitor.h))
    }
}

/// 接收侧的帧重组器：按 `frame_id` 收集分片，全部到齐（或收到 LAST 且分片齐）时交付整帧。
///
/// 特性：
/// - 单帧/多帧都支持；分片乱序到达也能重组；
/// - 内存上限保护（默认 64 MiB），超限时丢弃最旧的在途帧；
/// - 只保留由首片（`pkt_idx == 0`）建立的帧，避免无限增长。
#[derive(Debug)]
pub struct FrameAssembler {
    in_flight: HashMap<u32, PartialFrame>,
    max_bytes: usize,
    bytes: usize,
    dropped: u64,
    completed: u64,
}

#[derive(Debug)]
struct PartialFrame {
    seq: u16,
    ts_us: u32,
    keyframe: bool,
    expected: u16,
    got: u16,
    parts: Vec<Option<Vec<u8>>>,
    first_seen_order: u64,
}

impl PartialFrame {
    fn bytes(&self) -> usize {
        self.parts
            .iter()
            .map(|p| p.as_ref().map_or(0, Vec::len))
            .sum()
    }
}

/// 重组完成的一整帧视频码流。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompleteFrame {
    /// 帧 ID。
    pub frame_id: u32,
    /// 帧序列号（发送端递增）。
    pub seq: u16,
    /// 采集时间戳（Unix 微秒，u32 截断）。
    pub ts_us: u32,
    /// 是否关键帧。
    pub keyframe: bool,
    /// 码流。
    pub data: Vec<u8>,
}

impl Default for FrameAssembler {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameAssembler {
    /// 默认内存上限 64 MiB。
    pub fn new() -> Self {
        Self {
            in_flight: HashMap::new(),
            max_bytes: 64 * 1024 * 1024,
            bytes: 0,
            dropped: 0,
            completed: 0,
        }
    }

    /// 自定义内存上限（字节）。
    pub fn with_max_bytes(max_bytes: usize) -> Self {
        let mut s = Self::new();
        s.max_bytes = max_bytes.max(1);
        s
    }

    /// 在途帧数。
    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    /// 因内存上限丢弃的在途帧数。
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// 已完成的帧数。
    pub fn completed(&self) -> u64 {
        self.completed
    }

    /// 喂入一个视频 DATAGRAM：返回完整帧（若本次凑齐）。
    pub fn push(
        &mut self,
        header: &VideoHeader,
        payload: &[u8],
    ) -> anyhow::Result<Option<CompleteFrame>> {
        anyhow::ensure!(
            header.chan == CHAN_VIDEO,
            "FrameAssembler 只接受视频通道，收到 chan={}",
            header.chan
        );
        anyhow::ensure!(
            header.pkt_cnt >= 1 && header.pkt_idx < header.pkt_cnt,
            "非法分片序号：pkt_idx={} pkt_cnt={}",
            header.pkt_idx,
            header.pkt_cnt
        );

        // 首片建立状态；pkt_cnt 变化视为发送端换了新帧，作废旧状态。
        let stale = match self.in_flight.get(&header.frame_id) {
            Some(state) => state.expected != header.pkt_cnt,
            None => true,
        };
        if stale {
            if let Some(old) = self.in_flight.remove(&header.frame_id) {
                self.bytes = self.bytes.saturating_sub(old.bytes());
                self.dropped += 1;
            }
            let order = self.completed + self.dropped;
            self.in_flight.insert(
                header.frame_id,
                PartialFrame {
                    seq: header.frame_seq,
                    ts_us: header.ts_us,
                    keyframe: header.is_key(),
                    expected: header.pkt_cnt,
                    got: 0,
                    parts: vec![None; header.pkt_cnt as usize],
                    first_seen_order: order,
                },
            );
        }

        // 收下本分片（重复分片不计数）。
        let mut added = 0usize;
        {
            let state = self
                .in_flight
                .get_mut(&header.frame_id)
                .expect("上面已插入或已存在");
            let slot = &mut state.parts[header.pkt_idx as usize];
            if slot.is_none() {
                *slot = Some(payload.to_vec());
                state.got += 1;
                added = payload.len();
            }
        }
        self.bytes += added;

        // 内存上限：优先丢最旧的在途帧（不动当前帧）。
        while self.bytes > self.max_bytes {
            let oldest = self
                .in_flight
                .iter()
                .filter(|(id, _)| **id != header.frame_id)
                .min_by_key(|(_, s)| s.first_seen_order)
                .map(|(id, s)| (*id, s.bytes()));
            match oldest {
                Some((id, len)) => {
                    self.in_flight.remove(&id);
                    self.bytes = self.bytes.saturating_sub(len);
                    self.dropped += 1;
                }
                None => break,
            }
        }

        let complete = self
            .in_flight
            .get(&header.frame_id)
            .is_some_and(|s| s.got == s.expected);
        if !complete {
            return Ok(None);
        }

        let frame = self.in_flight.remove(&header.frame_id).expect("刚判定存在");
        let len = frame.bytes();
        let mut data = Vec::with_capacity(len);
        for part in frame.parts {
            let part = part.ok_or_else(|| anyhow::anyhow!("帧分片缺失却判定完整"))?;
            data.extend_from_slice(&part);
        }
        self.bytes = self.bytes.saturating_sub(len);
        self.completed += 1;
        Ok(Some(CompleteFrame {
            frame_id: header.frame_id,
            seq: frame.seq,
            ts_us: frame.ts_us,
            keyframe: frame.keyframe,
            data,
        }))
    }
}

/// 把 [`VideoHeader`] 的 flags 转成「是否关键帧 / 是否最后一片」的便捷判断。
pub fn header_flags(header: &VideoHeader) -> (bool, bool) {
    (
        header.flags & VIDEO_FLAG_KEY != 0,
        header.flags & VIDEO_FLAG_LAST != 0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(frame_id: u32, pkt_idx: u16, pkt_cnt: u16, flags: u8) -> VideoHeader {
        VideoHeader {
            chan: CHAN_VIDEO,
            flags,
            frame_id,
            frame_seq: frame_id as u16,
            pkt_idx,
            pkt_cnt,
            ts_us: 1000 + frame_id,
        }
    }

    #[test]
    fn assembles_single_packet_frame() {
        let mut a = FrameAssembler::new();
        let f = a
            .push(&header(7, 0, 1, VIDEO_FLAG_KEY | VIDEO_FLAG_LAST), b"hello")
            .unwrap()
            .expect("单包帧应立即完成");
        assert_eq!(f.frame_id, 7);
        assert!(f.keyframe);
        assert_eq!(f.data, b"hello");
        assert_eq!(f.ts_us, 1007);
        assert_eq!(a.completed(), 1);
        assert_eq!(a.in_flight(), 0);
    }

    #[test]
    fn assembles_multi_packet_frame_out_of_order() {
        let mut a = FrameAssembler::new();
        // 先到 1、2 号分片，最后到 0 号（0 号不是 LAST，但三片凑齐即完成）
        assert!(a.push(&header(1, 1, 3, 0), b"b").unwrap().is_none());
        assert!(a
            .push(&header(1, 2, 3, VIDEO_FLAG_LAST), b"c")
            .unwrap()
            .is_none());
        let f = a
            .push(&header(1, 0, 3, 0), b"a")
            .unwrap()
            .expect("分片凑齐应立即完成");
        assert_eq!(f.data, b"abc");
        assert!(!f.keyframe);
        assert_eq!(a.completed(), 1);
        assert_eq!(a.in_flight(), 0);
    }

    #[test]
    fn duplicate_fragment_does_not_corrupt_frame() {
        let mut a = FrameAssembler::new();
        assert!(a
            .push(&header(2, 0, 2, VIDEO_FLAG_KEY), b"x")
            .unwrap()
            .is_none());
        assert!(a
            .push(&header(2, 0, 2, VIDEO_FLAG_KEY), b"x")
            .unwrap()
            .is_none());
        let f = a.push(&header(2, 1, 2, VIDEO_FLAG_LAST), b"y").unwrap();
        let f = f.expect("重复首片后仍应能完成");
        assert_eq!(f.data, b"xy");
        assert!(f.keyframe);
    }

    #[test]
    fn drops_oversized_in_flight_frames() {
        let mut a = FrameAssembler::with_max_bytes(8);
        // 每帧 5B：两帧共 10B，超过 8B 上限后必须丢弃最旧的在途帧
        a.push(&header(1, 0, 2, 0), b"aaaaa").unwrap();
        a.push(&header(2, 0, 2, 0), b"bbbbb").unwrap();
        assert!(a.dropped() >= 1, "超出内存上限应丢弃最旧的在途帧");
        assert!(a.in_flight() <= 2);
    }

    #[test]
    fn rejects_bad_packet_index() {
        let mut a = FrameAssembler::new();
        assert!(a.push(&header(1, 0, 0, 0), b"x").is_err());
        assert!(a.push(&header(1, 5, 5, 0), b"x").is_err());
    }
}
