// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! `dfy-codec` — 大肥鱼远控的编码/解码模块。
//!
//! Windows 后端基于 **Media Foundation**：用 `MFTEnumEx` 枚举系统里已注册的
//! H.264 编码/解码 MFT（硬编 NVENC/QSV/AMF 若存在会优先，否则 Microsoft
//! 软编 MFT 保底），再通过 `IMFTransform` 驱动。输出统一为 **Annex-B**
//! （3/4 字节 start code，SPS/PPS 随关键帧）；解码器同时接受 Annex-B 与
//! AVCC（4 字节长度前缀）。
//!
//! # 平台支持
//!
//! * Windows：完整实现（Media Foundation）。
//! * macOS：完整实现（VideoToolbox 硬件优先 H.264 编解码）。
//! * 其他平台：stub，构造器返回明确错误，保证工作区在非 Windows/macOS 上照常
//!   `cargo check`。
//!
//! # 输入像素格式
//!
//! Media Foundation 的 H.264 编码器只接受 **NV12**。DXGI 采集默认给 BGRA8，
//! 因此 [`VideoEncoder::encode`] 接受 BGRA8 / RGBA8 / NV12 三种布局：
//!
//! * 4 字节/像素（`stride >= width*4` 且 `raw.len()` 足够）按 **BGRA8** 处理
//!   （与 DXGI 桌面采集一致）；
//! * 否则按 NV12 处理（Y 平面 `stride` 字节/行，UV 平面同样 stride）。
//!
//! 需要显式指定格式（例如 RGBA8）时用 [`create_encoder_with_format`]。
//!
//! # 线程模型
//!
//! [`VideoEncoder`]/[`VideoDecoder`] 都是 `Send`：Media Foundation 以 MTA
//! （multithreaded apartment）运行，可在工作线程间移动。每个对象内部各自
//! 持有一份 MF 引用计数（[`MFStartup`]/[`MFShutdown`] 配对），进程内可安全
//! 地创建/销毁多个编码器、解码器。

#![warn(missing_docs)]

use std::fmt;

pub mod format;
mod nal;

#[cfg(windows)]
mod mf;
#[cfg(target_os = "macos")]
mod vt;

#[cfg(not(any(windows, target_os = "macos")))]
mod stub;

/// 纯 Rust JPEG/MJPEG 兜底编解码（所有平台可用）。
pub mod jpeg;

// 常用转换函数直接暴露在 crate 根，方便 dfy-session 等调用方使用。
pub use format::{bgra_to_nv12, bgra_to_nv12_into, rgba_to_nv12, rgba_to_nv12_into};

/// 视频编码格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    /// H.264 / AVC（Windows 上走 Media Foundation，硬件优先）。
    H264,
    /// H.265 / HEVC（v1 只做能力探测，编码/解码返回 `Unsupported`）。
    H265,
    /// MJPEG（逐帧 JPEG）：纯 Rust、跨平台兜底，每帧都是关键帧。
    Mjpeg,
}

impl Codec {
    /// 便于日志的短名称。
    pub const fn as_str(self) -> &'static str {
        match self {
            Codec::H264 => "H264",
            Codec::H265 => "H265",
            Codec::Mjpeg => "MJPEG",
        }
    }
}

impl fmt::Display for Codec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 编码器可接受的输入像素格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PixelFormatIn {
    /// 4 字节/像素，字节序 B, G, R, A（DXGI 桌面采集默认）。
    Bgra8,
    /// 4 字节/像素，字节序 R, G, B, A。
    Rgba8,
    /// 1.5 字节/像素，Y 平面 + 交织 UV 平面（4:2:0）。
    Nv12,
}

/// 解码输出格式（v1 只有 NV12）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PixelFormatOut {
    /// Y 平面 + 交织 UV 平面（4:2:0），紧凑排列。
    Nv12,
}

impl fmt::Display for PixelFormatOut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PixelFormatOut::Nv12 => f.write_str("NV12"),
        }
    }
}

/// 最小目标帧率（fps）。
pub const MIN_FPS: u32 = 1;
/// 最大目标帧率（fps）。高于此值的采集/编码节奏不作为受支持配置接受。
pub const MAX_FPS: u32 = 120;
/// 最小目标码率（kbps）。
pub const MIN_BITRATE_KBPS: u32 = 100;
/// 最大目标码率（kbps，即 100 Mbps）。
pub const MAX_BITRATE_KBPS: u32 = 100_000;

/// 编码器配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderConfig {
    /// 目标编码格式（v1 仅 `H264` 可用）。
    pub codec: Codec,
    /// 编码宽度（像素，需为偶数）。
    pub width: u32,
    /// 编码高度（像素，需为偶数）。
    pub height: u32,
    /// 目标帧率（fps，1..=120）。
    pub fps: u32,
    /// 目标码率（kbps，100..=100_000）。
    pub bitrate_kbps: u32,
    /// 关键帧间隔（帧数）。
    pub keyframe_interval: u32,
    /// 是否优先尝试硬件编码 MFT（失败自动回退软编）。
    pub prefer_hardware: bool,
}

impl EncoderConfig {
    /// 校验配置是否自洽（宽高非零且为偶数、fps/码率/GOP 在支持范围内）。
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.width == 0 || self.height == 0 {
            anyhow::bail!(
                "dfy-codec: 编码尺寸不能为 0（{}x{}）",
                self.width,
                self.height
            );
        }
        if !self.width.is_multiple_of(2) || !self.height.is_multiple_of(2) {
            anyhow::bail!(
                "dfy-codec: NV12 要求宽高为偶数（{}x{}）",
                self.width,
                self.height
            );
        }
        validate_fps(self.fps)?;
        validate_bitrate_kbps(self.bitrate_kbps)?;
        if self.keyframe_interval == 0 {
            anyhow::bail!("dfy-codec: 关键帧间隔不能为 0");
        }
        if self.codec == Codec::H265 {
            anyhow::bail!("dfy-codec: v1 不支持 H.265 编码（仅提供 H.264）");
        }
        Ok(())
    }

    /// 每帧时长（微秒，向下取整）。配置非法时返回 0，调用方应先校验。
    pub const fn frame_duration_us(&self) -> u64 {
        if self.fps == 0 || self.fps > MAX_FPS {
            0
        } else {
            1_000_000 / self.fps as u64
        }
    }
}

/// 校验受支持的目标帧率。
pub fn validate_fps(fps: u32) -> anyhow::Result<()> {
    anyhow::ensure!(
        (MIN_FPS..=MAX_FPS).contains(&fps),
        "dfy-codec: fps 必须在 {MIN_FPS}..={MAX_FPS} 之间（收到 {fps}）"
    );
    Ok(())
}

/// 校验受支持的目标码率（kbps）。
pub fn validate_bitrate_kbps(kbps: u32) -> anyhow::Result<()> {
    anyhow::ensure!(
        (MIN_BITRATE_KBPS..=MAX_BITRATE_KBPS).contains(&kbps),
        "dfy-codec: 码率必须在 {MIN_BITRATE_KBPS}..={MAX_BITRATE_KBPS} kbps 之间（收到 {kbps}）"
    );
    Ok(())
}

/// 编码器产生的附加码流信息（SPS/PPS 等），供封装/传输层使用。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodecExtra {
    /// H.264 SPS NAL 载荷（不含 start code）。
    pub sps: Option<Vec<u8>>,
    /// H.264 PPS NAL 载荷（不含 start code）。
    pub pps: Option<Vec<u8>>,
    /// AVCC 形式的 codec private（`avcC` 记录，可直接放进 mp4 的 avcC box
    /// 或用于 RTP 打包）。
    pub codec_private: Option<Vec<u8>>,
}

/// 一帧编码结果（Annex-B 字节流）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedFrame {
    /// Annex-B 码流（含 start code）。
    pub data: Vec<u8>,
    /// 是否为关键帧（含 IDR / SPS）。
    pub is_keyframe: bool,
    /// 展示时间戳（微秒），由调用方传入。
    pub pts_us: u64,
    /// SPS/PPS 等附加信息（首个关键帧后填充）。
    pub extra: CodecExtra,
}

/// 编码器抽象。
///
/// 由 [`create_encoder`] 创建；所有方法都可在任意线程调用（内部不做跨线程
/// 共享，`Send` 即可在线程间移动）。
pub trait VideoEncoder: Send {
    /// 编码一帧原始像素，返回本次产生的一个或多个码流帧。
    ///
    /// * `raw`：像素数据。4 字节/像素时按 BGRA8 处理（可用
    ///   [`create_encoder_with_format`] 显式指定 RGBA8），否则按 NV12。
    /// * `stride`：行字节数。BGRA/RGBA 为一行字节数（可含 padding）；
    ///   NV12 为 Y 平面行字节数，UV 平面假定同 stride。
    /// * `pts_us`：展示时间戳（微秒），会写入输出帧。
    fn encode(&mut self, raw: &[u8], stride: u32, pts_us: u64)
        -> anyhow::Result<Vec<EncodedFrame>>;

    /// 动态调整目标码率（kbps）；底层不支持时返回明确错误。
    ///
    /// 参数必须先通过 [`validate_bitrate_kbps`]。实现不得把不支持的控制当成成功。
    fn set_bitrate(&mut self, kbps: u32) -> anyhow::Result<()>;

    /// 动态调整目标帧率（fps）；底层不支持时返回明确错误。
    ///
    /// 参数必须先通过 [`validate_fps`]。采集侧限帧与编码器内部帧率控制是两件事；
    /// 需要重建媒体类型的实现应指明调用方改用新编码器，而不是静默接受。
    fn set_frame_rate(&mut self, _fps: u32) -> anyhow::Result<()> {
        anyhow::bail!("dfy-codec: 当前编码器不支持动态帧率调整，需要以新配置重建编码器")
    }

    /// 返回编码器当前生效的配置；无法提供时返回 `None`。
    fn config(&self) -> Option<&EncoderConfig> {
        None
    }

    /// 请求下一帧编码为关键帧（IDR）。
    fn request_keyframe(&mut self);

    /// 冲刷编码器内部缓冲，取出尚未输出的码流帧。
    ///
    /// 部分编码 MFT（如 Windows 自带的 H.264 软编）内部有十几帧的前瞻缓冲，
    /// 停止采集或写文件结束前必须调用一次，否则尾部帧会丢失。
    /// 默认实现不产生任何输出（适用于无缓冲的编码器实现）。
    fn flush(&mut self) -> anyhow::Result<Vec<EncodedFrame>> {
        Ok(Vec::new())
    }
}

/// 一帧解码结果（NV12）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedFrame {
    /// NV12 像素数据（Y 平面 + 交织 UV 平面），长度约 `stride*height*3/2`。
    pub data: Vec<u8>,
    /// 行字节数（Y 平面；UV 平面通常与之相同）。
    pub stride: u32,
    /// 帧宽（像素）。
    pub width: u32,
    /// 帧高（像素）。
    pub height: u32,
    /// 像素格式（v1 恒为 NV12）。
    pub format: PixelFormatOut,
    /// 展示时间戳（微秒）。解码器无外部时间戳输入，按 30fps 递增估计。
    pub pts_us: u64,
}

/// 解码器抽象。
pub trait VideoDecoder: Send {
    /// 送入一个访问单元（Annex-B 或 AVCC 均可），返回本次解出的帧。
    ///
    /// 解码器有内部重排序/延迟，第一帧可能返回空 `Vec`；调用方应持续送入。
    fn decode(&mut self, data: &[u8]) -> anyhow::Result<Vec<DecodedFrame>>;

    /// 冲刷解码器内部缓冲，返回所有剩余的待输出帧。
    ///
    /// 解码 MFT 有重排序缓冲（部分实现甚至要攒够十几帧才吐第一帧），
    /// 停止播放/文件解码结束前应调用一次。默认实现返回空（无缓冲）。
    fn flush(&mut self) -> anyhow::Result<Vec<DecodedFrame>> {
        Ok(Vec::new())
    }
}

/// 系统里探测到的一个编码器 MFT。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderInfo {
    /// MFT 的友好名称（`MFT_FRIENDLY_NAME_Attribute`，取不到时为 CLSID 字符串）。
    pub name: String,
    /// 该编码器支持的编码格式。
    pub codec: Codec,
    /// 是否为硬件编码 MFT（带 `MFT_ENUM_HARDWARE_VENDOR_ID_Attribute`）。
    pub hardware: bool,
    /// 当前是否可用（能成功 Activate 出 `IMFTransform`）。
    pub available: bool,
}

/// 创建编码器。
///
/// `H.265` 与非法配置返回明确错误；硬件编码 MFT 初始化失败会自动回退到
/// 软件编码。输入像素格式按 [`VideoEncoder::encode`] 的规则自动判定
/// （4 字节/像素 → BGRA8）。
pub fn create_encoder(cfg: &EncoderConfig) -> anyhow::Result<Box<dyn VideoEncoder>> {
    cfg.validate()?;
    if cfg.codec == Codec::Mjpeg {
        return Ok(Box::new(jpeg::JpegEncoder::new(cfg)?));
    }
    let enc = backend::create_encoder(cfg, None)?;
    Ok(enc)
}

/// 创建编码器并显式指定输入像素格式。
///
/// 与 [`create_encoder`] 相同，但跳过自动判定：例如采集层给 RGBA8 时传
/// [`PixelFormatIn::Rgba8`]。
pub fn create_encoder_with_format(
    cfg: &EncoderConfig,
    format: PixelFormatIn,
) -> anyhow::Result<Box<dyn VideoEncoder>> {
    cfg.validate()?;
    if cfg.codec == Codec::Mjpeg {
        // MJPEG 后端按数据布局自动识别 BGRA8 / NV12，无需显式格式。
        let _ = format;
        return Ok(Box::new(jpeg::JpegEncoder::new(cfg)?));
    }
    let enc = backend::create_encoder(cfg, Some(format))?;
    Ok(enc)
}

/// 创建解码器。
///
/// v1 仅支持 H.264；`prefer_hardware` 为真时优先尝试硬件解码 MFT（通常需要
/// D3D11 设备，失败自动回退软解）。
pub fn create_decoder(
    codec: Codec,
    prefer_hardware: bool,
) -> anyhow::Result<Box<dyn VideoDecoder>> {
    match codec {
        Codec::H264 => backend::create_decoder(prefer_hardware),
        Codec::H265 => anyhow::bail!("dfy-codec: v1 不支持 H.265 解码（仅提供 H.264）"),
        Codec::Mjpeg => Ok(Box::new(jpeg::JpegDecoder::new())),
    }
}

/// 枚举本机可用的 H.264 编码器（硬编 + 软编）。
///
/// 该函数不会失败：MF 初始化失败或系统无任何 MFT 时返回空列表（并打印一条
/// 警告日志）。
pub fn available_encoders() -> Vec<EncoderInfo> {
    backend::available_encoders()
}

/// 后端实现模块导出（平台相关）。
#[cfg(windows)]
use mf as backend;
#[cfg(not(any(windows, target_os = "macos")))]
use stub as backend;
#[cfg(target_os = "macos")]
use vt as backend;

/// crate 版本，便于握手/诊断日志。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_validation() {
        let good = EncoderConfig {
            codec: Codec::H264,
            width: 1280,
            height: 720,
            fps: 30,
            bitrate_kbps: 4000,
            keyframe_interval: 60,
            prefer_hardware: false,
        };
        assert!(good.validate().is_ok());
        assert_eq!(good.frame_duration_us(), 33_333);

        let mut high_fps = good.clone();
        high_fps.fps = 120;
        assert!(high_fps.validate().is_ok());
        assert_eq!(high_fps.frame_duration_us(), 8_333);

        let mut low_fps = good.clone();
        low_fps.fps = 1;
        assert!(low_fps.validate().is_ok());

        let mut bad = good.clone();
        bad.width = 1281;
        assert!(bad.validate().is_err());

        let mut bad = good.clone();
        bad.codec = Codec::H265;
        assert!(bad.validate().is_err());

        let mut bad = good.clone();
        bad.fps = 0;
        assert!(bad.validate().is_err());
        let mut bad = good.clone();
        bad.fps = 121;
        assert!(bad.validate().is_err());
        assert_eq!(bad.frame_duration_us(), 0);
        let mut bad = good.clone();
        bad.bitrate_kbps = 0;
        assert!(bad.validate().is_err());
        let mut bad = good.clone();
        bad.bitrate_kbps = MIN_BITRATE_KBPS - 1;
        assert!(bad.validate().is_err());
        let mut bad = good.clone();
        bad.bitrate_kbps = MAX_BITRATE_KBPS + 1;
        assert!(bad.validate().is_err());
        assert!(EncoderConfig {
            bitrate_kbps: MIN_BITRATE_KBPS,
            ..good.clone()
        }
        .validate()
        .is_ok());
        assert!(EncoderConfig {
            bitrate_kbps: MAX_BITRATE_KBPS,
            ..good.clone()
        }
        .validate()
        .is_ok());
        let mut bad = good;
        bad.keyframe_interval = 0;
        assert!(bad.validate().is_err());
    }

    #[test]
    fn h265_is_reported_unsupported() {
        let cfg = EncoderConfig {
            codec: Codec::H265,
            width: 640,
            height: 480,
            fps: 30,
            bitrate_kbps: 1000,
            keyframe_interval: 30,
            prefer_hardware: false,
        };
        let err = match create_encoder(&cfg) {
            Ok(_) => panic!("H.265 编码应返回 Unsupported"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("H.265"), "err = {err}");
        assert!(create_decoder(Codec::H265, false).is_err());
    }

    #[test]
    fn mjpeg_rejects_unsupported_dynamic_controls_instead_of_noop_success() {
        let cfg = EncoderConfig {
            codec: Codec::Mjpeg,
            width: 64,
            height: 48,
            fps: 120,
            bitrate_kbps: 1000,
            keyframe_interval: 1,
            prefer_hardware: false,
        };
        let mut enc = create_encoder(&cfg).expect("MJPEG 编码器应可用");
        assert_eq!(enc.config(), Some(&cfg));
        let bitrate_err = enc.set_bitrate(2000).unwrap_err().to_string();
        assert!(bitrate_err.contains("MJPEG") && bitrate_err.contains("无法动态设置"));
        assert!(enc.set_bitrate(0).is_err());
        let fps_err = enc.set_frame_rate(60).unwrap_err().to_string();
        assert!(fps_err.contains("不支持动态帧率") && fps_err.contains("重建"));
        assert!(enc.set_frame_rate(121).is_err());
    }

    #[test]
    fn codec_display() {
        assert_eq!(Codec::H264.to_string(), "H264");
        assert_eq!(PixelFormatOut::Nv12.to_string(), "NV12");
    }
}
