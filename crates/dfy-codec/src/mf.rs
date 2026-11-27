// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! Windows Media Foundation 后端：MFT 枚举 + `IMFTransform` 驱动。
//!
//! 设计要点（实测踩坑记录见 crate 根目录 README）：
//!
//! * Media Foundation 用**进程级引用计数**启动/关闭：每创建一个编码器/解码器
//!   就 `MFStartup` 一次，Drop 时 `MFShutdown` 一次；首次启动时顺带
//!   `CoIncrementMTAUsage`（常驻到进程结束），保证调用方即使是 STA 也能用 MFT。
//! * 编码器输入固定 NV12、输出 H.264 Annex-B；系统自带 MFT 的 SPS/PPS 从
//!   `MF_MT_MPEG_SEQUENCE_HEADER`（avcC 记录）读取，读不到就从首个关键帧提取。
//! * 硬件 MFT（NVENC/QSV/AMF）优先尝试；初始化/喂帧失败自动回退软编。
//! * 异步 MFT（`MF_TRANSFORM_ASYNC`）用 `MF_TRANSFORM_ASYNC_UNLOCK` 解锁，
//!   以同步方式驱动（`ProcessOutput` 返回 `MF_E_TRANSFORM_NEED_MORE_INPUT` 即止）。

use std::mem::ManuallyDrop;
use std::sync::{Mutex, OnceLock};

use anyhow::{anyhow, bail, Context, Result};
use windows::core::{Interface, GUID, PWSTR};
use windows::Win32::Foundation::VARIANT_BOOL;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoIncrementMTAUsage, CoTaskMemFree};
use windows::Win32::System::Variant::{
    VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_UI4,
};

use crate::format::{bgra_to_nv12_into, nv12_len, rgba_to_nv12_into};
use crate::nal;
use crate::{
    validate_bitrate_kbps, validate_fps, Codec, CodecExtra, DecodedFrame, EncodedFrame,
    EncoderConfig, EncoderInfo, PixelFormatIn, PixelFormatOut, VideoDecoder, VideoEncoder,
};

// ---------------------------------------------------------------------------
// MFStartup / MFShutdown 进程级引用计数
// ---------------------------------------------------------------------------

struct MfGlobal {
    refcount: usize,
    /// 是否已经持有进程级 MTA（`CoIncrementMTAUsage` 的 cookie 故意不释放，
    /// 保证整个进程生命周期内 MTA 都在）。
    mta_held: bool,
}

fn mf_global() -> &'static Mutex<MfGlobal> {
    static GLOBAL: OnceLock<Mutex<MfGlobal>> = OnceLock::new();
    GLOBAL.get_or_init(|| {
        Mutex::new(MfGlobal {
            refcount: 0,
            mta_held: false,
        })
    })
}

/// 诊断日志开关：设置环境变量 `DFY_CODEC_DEBUG=1` 时打印 MFT 协商细节。
pub(crate) fn debug_enabled() -> bool {
    std::env::var_os("DFY_CODEC_DEBUG").is_some()
}

/// 一次 MF 生命周期引用（编码器/解码器各持一份）。
struct MfSession {
    active: bool,
}

impl MfSession {
    fn acquire() -> Result<MfSession> {
        let mut g = mf_global().lock().unwrap_or_else(|e| e.into_inner());
        if g.refcount == 0 {
            unsafe {
                // 保证进程里有 MTA：调用方可能已把线程初始化成 STA，此时
                // CoInitializeEx(MTA) 会返回 RPC_E_CHANGED_MODE，但
                // CoIncrementMTAUsage 仍能创建/保持一个 MTA。
                if !g.mta_held {
                    let _ = CoIncrementMTAUsage().context("CoIncrementMTAUsage 失败")?;
                    g.mta_held = true;
                }
                MFStartup(MF_VERSION, MFSTARTUP_LITE).context("MFStartup 失败")?;
            }
        }
        g.refcount += 1;
        Ok(MfSession { active: true })
    }
}

impl Clone for MfSession {
    /// 克隆等于再持有一份引用（对应一次 `MFStartup` 的引用计数）。
    fn clone(&self) -> MfSession {
        if self.active {
            let mut g = mf_global().lock().unwrap_or_else(|e| e.into_inner());
            g.refcount += 1;
        }
        MfSession {
            active: self.active,
        }
    }
}

impl Drop for MfSession {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let mut g = mf_global().lock().unwrap_or_else(|e| e.into_inner());
        g.refcount = g.refcount.saturating_sub(1);
        if g.refcount == 0 {
            unsafe {
                let _ = MFShutdown();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// MFT 枚举
// ---------------------------------------------------------------------------

/// 一个候选 MFT（已激活前的 IMFActivate）。
struct MftCandidate {
    activate: IMFActivate,
    name: String,
    hardware: bool,
}

/// 读字符串属性：存在则返回 `Some`。
fn get_attr_string(attrs: &IMFAttributes, key: &GUID) -> Option<String> {
    unsafe {
        let mut p = PWSTR::null();
        let mut len = 0u32;
        if attrs.GetAllocatedString(key, &mut p, &mut len).is_err() {
            return None;
        }
        let s = if p.is_null() {
            String::new()
        } else {
            p.to_string().unwrap_or_default()
        };
        CoTaskMemFree(Some(p.0 as *const core::ffi::c_void));
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }
}

fn attr_exists(attrs: &IMFAttributes, key: &GUID) -> bool {
    unsafe { attrs.GetStringLength(key).is_ok() || attrs.GetBlobSize(key).is_ok() }
}

/// `MFTEnumEx` 封装：按输入/输出 subtype 过滤，返回候选列表。
fn enumerate_mfts(
    category: GUID,
    input_subtype: GUID,
    output_subtype: GUID,
) -> Result<Vec<MftCandidate>> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: input_subtype,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: output_subtype,
    };
    // 只列同步/异步 + 硬件，并按 merit 排序过滤。
    let flags = MFT_ENUM_FLAG(
        MFT_ENUM_FLAG_SYNCMFT.0
            | MFT_ENUM_FLAG_ASYNCMFT.0
            | MFT_ENUM_FLAG_HARDWARE.0
            | MFT_ENUM_FLAG_SORTANDFILTER.0,
    );
    let mut array: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        MFTEnumEx(
            category,
            flags,
            Some(&input),
            Some(&output),
            &mut array,
            &mut count,
        )
        .context("MFTEnumEx 失败")?;
    }
    if array.is_null() || count == 0 {
        if !array.is_null() {
            unsafe { CoTaskMemFree(Some(array as *const core::ffi::c_void)) };
        }
        return Ok(Vec::new());
    }
    let acts: Vec<Option<IMFActivate>> =
        unsafe { std::slice::from_raw_parts(array, count as usize).to_vec() };
    unsafe { CoTaskMemFree(Some(array as *const core::ffi::c_void)) };

    let mut out = Vec::new();
    for act in acts.into_iter().flatten() {
        let attrs: Option<IMFAttributes> = act.cast().ok();
        let name = attrs
            .as_ref()
            .and_then(|a| get_attr_string(a, &MFT_FRIENDLY_NAME_Attribute))
            .unwrap_or_else(|| "<未命名 MFT>".to_string());
        let hardware = attrs
            .as_ref()
            .map(|a| attr_exists(a, &MFT_ENUM_HARDWARE_VENDOR_ID_Attribute))
            .unwrap_or(false);
        if debug_enabled() {
            let clsid = attrs
                .as_ref()
                .and_then(|a| unsafe { a.GetGUID(&MFT_TRANSFORM_CLSID_Attribute) }.ok())
                .map(|g| {
                    format!(
                        "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
                        g.data1, g.data2, g.data3,
                        g.data4[0], g.data4[1], g.data4[2], g.data4[3],
                        g.data4[4], g.data4[5], g.data4[6], g.data4[7]
                    )
                })
                .unwrap_or_else(|| "<无 CLSID>".to_string());
            let url = attrs
                .as_ref()
                .and_then(|a| get_attr_string(a, &MFT_ENUM_HARDWARE_URL_Attribute))
                .unwrap_or_default();
            eprintln!(
                "dfy-codec[debug]: 枚举 MFT「{}」硬件={} CLSID={} URL={}",
                name, hardware, clsid, url
            );
        }
        out.push(MftCandidate {
            activate: act,
            name,
            hardware,
        });
    }
    Ok(out)
}

/// 按偏好排序：`prefer_hardware` 为真时硬件在前，否则保持枚举顺序（merit 排序）。
fn sort_candidates(mut list: Vec<MftCandidate>, prefer_hardware: bool) -> Vec<MftCandidate> {
    if prefer_hardware {
        list.sort_by_key(|c| !c.hardware);
    } else if list.iter().any(|c| c.hardware) {
        // 明确不优先硬件时把软编排前面，避免硬件 MFT 初始化失败浪费启动时间。
        list.sort_by_key(|c| c.hardware);
    }
    list
}

// ---------------------------------------------------------------------------
// 媒体类型构造
// ---------------------------------------------------------------------------

fn media_attrs(t: &IMFMediaType) -> Result<IMFAttributes> {
    t.cast().context("IMFMediaType → IMFAttributes 转换失败")
}

fn pack_ratio(a: u32, b: u32) -> u64 {
    ((a as u64) << 32) | b as u64
}

/// 编码器输入类型：NV12 + 帧尺寸/帧率/逐行/码率。
fn build_encoder_input_type(cfg: &EncoderConfig) -> Result<IMFMediaType> {
    unsafe {
        let t = MFCreateMediaType().context("MFCreateMediaType 失败")?;
        let a = media_attrs(&t)?;
        a.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        a.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        a.SetUINT64(&MF_MT_FRAME_SIZE, pack_ratio(cfg.width, cfg.height))?;
        a.SetUINT64(&MF_MT_FRAME_RATE, pack_ratio(cfg.fps, 1))?;
        a.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_ratio(1, 1))?;
        a.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        a.SetUINT32(&MF_MT_AVG_BITRATE, cfg.bitrate_kbps.saturating_mul(1000))?;
        Ok(t)
    }
}

/// 补全 MFT 自己给出的类型：写入帧尺寸/帧率/逐行/码率等本编码器需要的属性。
fn patch_video_type(t: &IMFMediaType, cfg: &EncoderConfig, is_output: bool) -> Result<()> {
    let a = media_attrs(t)?;
    unsafe {
        a.SetUINT64(&MF_MT_FRAME_SIZE, pack_ratio(cfg.width, cfg.height))?;
        a.SetUINT64(&MF_MT_FRAME_RATE, pack_ratio(cfg.fps, 1))?;
        a.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_ratio(1, 1))?;
        a.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        a.SetUINT32(&MF_MT_AVG_BITRATE, cfg.bitrate_kbps.saturating_mul(1000))?;
        if is_output {
            let _ = a.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32);
        }
    }
    Ok(())
}

/// 从 MFT 自己声明的可用类型里找一个指定 subtype 的类型。
///
/// 硬件 MFT（QSV/NVENC）对媒体类型要求更严格，自己拼的类型常被拒绝，
/// 这时退回 MFT 给的类型再补属性成功率更高。
fn find_available_type(
    transform: &IMFTransform,
    output: bool,
    subtype: &GUID,
    cfg: &EncoderConfig,
) -> Option<IMFMediaType> {
    for i in 0..16u32 {
        let t = unsafe {
            if output {
                transform.GetOutputAvailableType(0, i)
            } else {
                transform.GetInputAvailableType(0, i)
            }
        };
        let Ok(t) = t else { break };
        let Ok(a) = media_attrs(&t) else { continue };
        let major = unsafe { a.GetGUID(&MF_MT_MAJOR_TYPE) }.unwrap_or_default();
        if major != MFMediaType_Video {
            continue;
        }
        let sub = unsafe { a.GetGUID(&MF_MT_SUBTYPE) }.unwrap_or_default();
        if sub != *subtype {
            continue;
        }
        if debug_enabled() {
            let size = unsafe { a.GetUINT64(&MF_MT_FRAME_SIZE) }.unwrap_or(0);
            let kind = if output { "输出" } else { "输入" };
            eprintln!(
                "dfy-codec[debug]: MFT 声明的可用{kind}类型 #{i} 命中，FRAME_SIZE=0x{size:016x}"
            );
        }
        if patch_video_type(&t, cfg, output).is_ok() {
            return Some(t);
        }
    }
    None
}

/// 协商编码器输入/输出媒体类型。
///
/// 依次尝试「本 crate 构造的类型」和「MFT 自己声明的类型」的组合，
/// 每次尝试前先清空类型，避免上一次失败留下半配置状态。
fn configure_encoder_types(transform: &IMFTransform, cfg: &EncoderConfig) -> Result<()> {
    let built_in = build_encoder_input_type(cfg)?;
    let built_out = build_encoder_output_type(cfg)?;
    let avail_in = find_available_type(transform, false, &MFVideoFormat_NV12, cfg);
    let avail_out = find_available_type(transform, true, &MFVideoFormat_H264, cfg);

    let ins: Vec<&IMFMediaType> = std::iter::once(&built_in).chain(avail_in.iter()).collect();
    let outs: Vec<&IMFMediaType> = std::iter::once(&built_out)
        .chain(avail_out.iter())
        .collect();

    let mut last_err = String::new();
    for out in &outs {
        for inp in &ins {
            unsafe {
                let _ = transform.SetOutputType(0, None::<&IMFMediaType>, 0);
                let _ = transform.SetInputType(0, None::<&IMFMediaType>, 0);
                let r = transform
                    .SetOutputType(0, *out, 0)
                    .and_then(|_| transform.SetInputType(0, *inp, 0));
                match r {
                    Ok(()) => {
                        if debug_enabled() {
                            eprintln!(
                                "dfy-codec[debug]: 媒体类型协商成功（自定义类型={}）",
                                std::ptr::eq(*out, &built_out) as u8
                            );
                        }
                        return Ok(());
                    }
                    Err(e) => {
                        last_err = format!("0x{:08x} {}", e.code().0 as u32, e.message());
                    }
                }
            }
        }
    }
    bail!("dfy-codec: 编码 MFT 拒绝了所有输入/输出类型组合（最后错误：{last_err}）");
}

/// 编码器输出类型：H.264 Main profile。
fn build_encoder_output_type(cfg: &EncoderConfig) -> Result<IMFMediaType> {
    unsafe {
        let t = MFCreateMediaType().context("MFCreateMediaType 失败")?;
        let a = media_attrs(&t)?;
        a.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        a.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        a.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)?;
        a.SetUINT64(&MF_MT_FRAME_SIZE, pack_ratio(cfg.width, cfg.height))?;
        a.SetUINT64(&MF_MT_FRAME_RATE, pack_ratio(cfg.fps, 1))?;
        a.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_ratio(1, 1))?;
        a.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        a.SetUINT32(&MF_MT_AVG_BITRATE, cfg.bitrate_kbps.saturating_mul(1000))?;
        Ok(t)
    }
}

/// 解码器输入类型：H.264（帧尺寸等由解码器从码流里解析）。
fn build_decoder_input_type() -> Result<IMFMediaType> {
    unsafe {
        let t = MFCreateMediaType().context("MFCreateMediaType 失败")?;
        let a = media_attrs(&t)?;
        a.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        a.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        Ok(t)
    }
}

// ---------------------------------------------------------------------------
// VARIANT / ICodecAPI 小工具
// ---------------------------------------------------------------------------

fn variant_u32(v: u32) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_UI4,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { ulVal: v },
            }),
        },
    }
}

fn variant_bool(v: bool) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_BOOL,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 {
                    boolVal: VARIANT_BOOL(if v { -1 } else { 0 }),
                },
            }),
        },
    }
}

/// 仅当 MFT 声明支持该 Codec API 时才写值；返回是否写入成功。
fn set_codec_value(api: &ICodecAPI, key: &GUID, value: &VARIANT) -> bool {
    unsafe {
        if api.IsSupported(key).is_err() {
            return false;
        }
        api.SetValue(key, value).is_ok()
    }
}

/// 与 `set_codec_value` 相同，但会记录 MFT 是否声明支持（调试用）。
fn apply_codec_value(api: &ICodecAPI, key: &GUID, value: &VARIANT, tag: &str) -> bool {
    let supported = unsafe { api.IsSupported(key).is_ok() };
    let mut applied = false;
    if supported {
        match unsafe { api.SetValue(key, value) } {
            Ok(()) => applied = true,
            Err(e) if debug_enabled() => {
                eprintln!(
                    "dfy-codec[debug]: CodecAPI {tag} SetValue 失败 hresult=0x{:08x}",
                    e.code().0 as u32
                );
            }
            Err(_) => {}
        }
    }
    if debug_enabled() {
        eprintln!("dfy-codec[debug]: CodecAPI {tag} 支持={supported} 写入={applied}");
    }
    applied
}

/// 低延迟 CBR 参数（设置失败不致命：不同 MFT 支持度不同）。
fn apply_encoder_codec_api(api: &ICodecAPI, cfg: &EncoderConfig) {
    let bitrate = cfg.bitrate_kbps.saturating_mul(1000);
    // 调试用：DFY_CODEC_RC 覆盖码控模式（0=CBR 1=PeakVBR 2=UnconstrainedVBR 3=Quality 4=LowDelayVBR）。
    let rc = std::env::var("DFY_CODEC_RC")
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .unwrap_or(eAVEncCommonRateControlMode_CBR.0);
    let _ = apply_codec_value(
        api,
        &CODECAPI_AVEncCommonLowLatency,
        &variant_bool(true),
        "AVEncCommonLowLatency",
    );
    let _ = apply_codec_value(
        api,
        &CODECAPI_AVEncCommonRateControlMode,
        &variant_u32(rc as u32),
        "AVEncCommonRateControlMode",
    );
    // 无 B 帧 → 零重排序延迟。
    let _ = apply_codec_value(
        api,
        &CODECAPI_AVEncMPVDefaultBPictureCount,
        &variant_u32(0),
        "AVEncMPVDefaultBPictureCount=0",
    );
    let _ = apply_codec_value(
        api,
        &CODECAPI_AVEncCommonMeanBitRate,
        &variant_u32(bitrate),
        "AVEncCommonMeanBitRate",
    );
    let _ = apply_codec_value(
        api,
        &CODECAPI_AVEncCommonMaxBitRate,
        &variant_u32(bitrate),
        "AVEncCommonMaxBitRate",
    );
    let _ = apply_codec_value(
        api,
        &CODECAPI_AVEncMPVGOPSize,
        &variant_u32(cfg.keyframe_interval),
        "AVEncMPVGOPSize",
    );
}

// ---------------------------------------------------------------------------
// 采样 / 缓冲工具
// ---------------------------------------------------------------------------

/// 创建承载 `data` 的 IMFSample（带时间戳）。
fn make_sample(data: &[u8], sample_time_100ns: i64, duration_100ns: i64) -> Result<IMFSample> {
    if data.is_empty() {
        bail!("dfy-codec: 不能编码空帧");
    }
    unsafe {
        let buffer =
            MFCreateMemoryBuffer(data.len() as u32).context("MFCreateMemoryBuffer 失败")?;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        buffer
            .Lock(&mut ptr, None, None)
            .context("IMFMediaBuffer::Lock 失败")?;
        if ptr.is_null() {
            let _ = buffer.Unlock();
            bail!("dfy-codec: IMFMediaBuffer::Lock 返回空指针");
        }
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
        buffer.Unlock().context("IMFMediaBuffer::Unlock 失败")?;
        buffer
            .SetCurrentLength(data.len() as u32)
            .context("IMFMediaBuffer::SetCurrentLength 失败")?;
        let sample = MFCreateSample().context("MFCreateSample 失败")?;
        sample
            .AddBuffer(&buffer)
            .context("IMFSample::AddBuffer 失败")?;
        if sample_time_100ns >= 0 {
            let _ = sample.SetSampleTime(sample_time_100ns);
            let _ = sample.SetSampleDuration(duration_100ns);
        }
        Ok(sample)
    }
}

/// 拷贝一个 IMFSample 的全部字节。
fn copy_sample_data(sample: &IMFSample) -> Result<Vec<u8>> {
    unsafe {
        let buffer = match sample.ConvertToContiguousBuffer() {
            Ok(b) => b,
            Err(_) => sample
                .GetBufferByIndex(0)
                .context("IMFSample 里没有可用 buffer")?,
        };
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut cur_len = 0u32;
        buffer
            .Lock(&mut ptr, None, Some(&mut cur_len))
            .context("IMFMediaBuffer::Lock 失败")?;
        let data = if ptr.is_null() || cur_len == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(ptr, cur_len as usize).to_vec()
        };
        let _ = buffer.Unlock();
        Ok(data)
    }
}

fn sample_time_us(sample: &IMFSample) -> Option<u64> {
    unsafe {
        let t = sample.GetSampleTime().ok()?;
        if t < 0 {
            None
        } else {
            Some((t as u64) / 10)
        }
    }
}

/// 判断 HRESULT 是否为 `MF_E_TRANSFORM_NEED_MORE_INPUT`。
fn is_need_more_input(e: &windows::core::Error) -> bool {
    e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT
}

fn is_stream_change(e: &windows::core::Error) -> bool {
    e.code() == MF_E_TRANSFORM_STREAM_CHANGE
}

// ---------------------------------------------------------------------------
// 编码器
// ---------------------------------------------------------------------------

/// Media Foundation H.264 编码器。
pub(crate) struct MfEncoder {
    _session: MfSession,
    cfg: EncoderConfig,
    input_format: Option<PixelFormatIn>,
    activate: IMFActivate,
    transform: IMFTransform,
    codec_api: Option<ICodecAPI>,
    provides_samples: bool,
    out_capacity: u32,
    extra: CodecExtra,
    pending_keyframe: bool,
    nv12_buf: Vec<u8>,
    frame_duration_100ns: i64,
    current_pts_us: u64,
    frame_count: u64,
    /// 已喂入但尚未对应到输出的时间戳队列。
    ///
    /// MF 编码 MFT 的输出 sample 往往不带时间戳（实测 QSV/软编都返回 0），
    /// 所以 pts 以「喂入顺序」为准：每成功喂一帧就入队，每出一帧就出队。
    pending_pts: std::collections::VecDeque<u64>,
    /// 是否已经成功输出过帧（用于 extra 只需填一次）。
    seen_output: bool,
}

// SAFETY: 编码器内部只持有 MTA 上创建的 COM 对象；MTA 对象是自由线程的，
// 且结构体独占所有权（没有跨线程共享的可变别名）。
unsafe impl Send for MfEncoder {}

struct EncoderParts {
    transform: IMFTransform,
    codec_api: Option<ICodecAPI>,
    provides_samples: bool,
    out_capacity: u32,
    extra: CodecExtra,
}

/// 用给定候选 MFT 尝试建立可用的编码 transform。
fn setup_encoder_transform(cfg: &EncoderConfig, candidate: &MftCandidate) -> Result<EncoderParts> {
    // 在激活前把「允许异步 MFT」写到激活属性里：多数硬件 MFT（QSV/NVENC）
    // 是异步 MFT，未解锁时 SetInputType 会返回 0xC00D6D77
    // （MF_E_TRANSFORM_ASYNC_MFT_NOT_SUPPORTED "该调用方似乎不支持异步转换"）。
    unsafe {
        let unlock = candidate.activate.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1);
        if debug_enabled() {
            eprintln!(
                "dfy-codec[debug]: MFT「{}」激活前解锁 IMFActivate={:?}",
                candidate.name,
                unlock
                    .as_ref()
                    .err()
                    .map(|e| format!("0x{:08x}", e.code().0 as u32))
            );
        }
    }
    let transform: IMFTransform = unsafe { candidate.activate.ActivateObject() }
        .with_context(|| format!("激活编码 MFT「{}」失败", candidate.name))?;

    // 同时在 transform 自己的属性存储上解锁（官方文档要求的写法），
    // 对同步 MFT 写入该属性是无害的，所以不再先判断 MF_TRANSFORM_ASYNC。
    if let Ok(attrs) = transform.cast::<IMFAttributes>() {
        let set = unsafe { attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }.is_ok();
        if debug_enabled() {
            let is_async = unsafe { attrs.GetUINT32(&MF_TRANSFORM_ASYNC) }.unwrap_or(0);
            let d3d11_aware = unsafe { attrs.GetUINT32(&MF_SA_D3D11_AWARE) }.unwrap_or(0);
            eprintln!(
                "dfy-codec[debug]: MFT「{}」MF_TRANSFORM_ASYNC={is_async} 解锁写入={set} D3D11_AWARE={d3d11_aware}",
                candidate.name
            );
        }
    } else if debug_enabled() {
        eprintln!(
            "dfy-codec[debug]: MFT「{}」不暴露 IMFAttributes",
            candidate.name
        );
    }
    let codec_api: Option<ICodecAPI> = transform.cast().ok();

    // 部分 MFT（含微软软编）只在设置媒体类型前接受低延迟/码控属性，先试一遍。
    if let Some(api) = &codec_api {
        apply_encoder_codec_api(api, cfg);
    }

    configure_encoder_types(&transform, cfg)
        .with_context(|| format!("编码 MFT「{}」媒体类型协商失败", candidate.name))?;

    if let Some(api) = &codec_api {
        apply_encoder_codec_api(api, cfg);
    }

    unsafe {
        let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
        let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
    }

    let info = unsafe { transform.GetOutputStreamInfo(0) }.context("GetOutputStreamInfo 失败")?;
    let provides_samples = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
    // 有些 MFT 报 cbSize=0（变长输出），给一个足够大的保守容量。
    let out_capacity = info
        .cbSize
        .max(cfg.width.saturating_mul(cfg.height))
        .max(256 * 1024);

    // 从输出类型读取 SPS/PPS（avcC 记录）。
    let mut extra = CodecExtra::default();
    unsafe {
        if let Ok(cur) = transform.GetOutputCurrentType(0) {
            if let Ok(attrs) = cur.cast::<IMFAttributes>() {
                if let Ok(size) = attrs.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) {
                    if size > 0 {
                        let mut blob = vec![0u8; size as usize];
                        if attrs
                            .GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut blob, None)
                            .is_ok()
                        {
                            let (sps, pps) = nal::parse_avcc_config(&blob);
                            if sps.is_some() && pps.is_some() {
                                extra.codec_private = Some(blob);
                                extra.sps = sps;
                                extra.pps = pps;
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(EncoderParts {
        transform,
        codec_api,
        provides_samples,
        out_capacity,
        extra,
    })
}

impl MfEncoder {
    fn new(cfg: &EncoderConfig, input_format: Option<PixelFormatIn>) -> Result<Self> {
        let session = MfSession::acquire()?;
        let candidates = sort_candidates(
            enumerate_mfts(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFVideoFormat_NV12,
                MFVideoFormat_H264,
            )
            .context("枚举 H.264 编码器失败")?,
            cfg.prefer_hardware,
        );
        if candidates.is_empty() {
            bail!(
                "dfy-codec: 系统里没有注册任何 H.264 编码 MFT（Media Foundation 未安装编解码器？）"
            );
        }

        let mut errors = Vec::new();
        for cand in candidates {
            match setup_encoder_transform(cfg, &cand) {
                Ok(parts) => {
                    eprintln!(
                        "dfy-codec: 使用{} H.264 编码 MFT「{}」",
                        if cand.hardware { "硬件" } else { "软件" },
                        cand.name
                    );
                    let nv12_buf = vec![0u8; nv12_len(cfg.width, cfg.height).unwrap_or(0)];
                    return Ok(MfEncoder {
                        _session: session,
                        cfg: cfg.clone(),
                        input_format,
                        activate: cand.activate.clone(),
                        transform: parts.transform,
                        codec_api: parts.codec_api,
                        provides_samples: parts.provides_samples,
                        out_capacity: parts.out_capacity,
                        extra: parts.extra,
                        pending_keyframe: false,
                        nv12_buf,
                        frame_duration_100ns: (10_000_000 / cfg.fps as i64).max(1),
                        current_pts_us: 0,
                        frame_count: 0,
                        pending_pts: std::collections::VecDeque::new(),
                        seen_output: false,
                    });
                }
                Err(e) => {
                    if debug_enabled() {
                        eprintln!(
                            "dfy-codec[debug]: 编码候选「{}」初始化失败：{e:#}",
                            cand.name
                        );
                    }
                    errors.push(format!(
                        "{}（{}）: {e:#}",
                        cand.name,
                        if cand.hardware { "硬件" } else { "软件" }
                    ));
                }
            }
        }
        bail!(
            "dfy-codec: 所有 H.264 编码 MFT 初始化失败：\n  {}",
            errors.join("\n  ")
        )
    }

    /// 重建 transform（`CODECAPI_AVEncVideoForceKeyFrame` 不可用时的关键帧兜底）。
    fn rebuild(&mut self) -> Result<()> {
        let candidate = MftCandidate {
            activate: self.activate.clone(),
            name: "重建".to_string(),
            hardware: false,
        };
        let parts = setup_encoder_transform(&self.cfg, &candidate)?;
        self.transform = parts.transform;
        self.pending_pts.clear();
        self.codec_api = parts.codec_api;
        self.provides_samples = parts.provides_samples;
        self.out_capacity = parts.out_capacity;
        if self.extra.sps.is_none() {
            self.extra = parts.extra;
        }
        Ok(())
    }

    /// 根据输入长度与 stride 判定像素格式（与 crate 文档一致）。
    fn resolve_format(&self, raw: &[u8], stride: u32) -> PixelFormatIn {
        if let Some(f) = self.input_format {
            return f;
        }
        let w = self.cfg.width as usize;
        let h = self.cfg.height as usize;
        let stride = stride as usize;
        if stride >= w * 4 && raw.len() >= stride * h {
            PixelFormatIn::Bgra8
        } else {
            PixelFormatIn::Nv12
        }
    }

    /// 把调用方的 NV12 输入拷成紧凑 NV12（Y 平面与 UV 平面各自去掉 padding）。
    fn copy_nv12(&mut self, raw: &[u8], stride: u32) -> Result<()> {
        let w = self.cfg.width as usize;
        let h = self.cfg.height as usize;
        let stride = stride as usize;
        if stride < w {
            bail!("dfy-codec: NV12 stride {stride} 小于宽度 {w}");
        }
        let need = stride
            .checked_mul(h + h / 2)
            .ok_or_else(|| anyhow!("dfy-codec: NV12 尺寸溢出"))?;
        if raw.len() < need {
            bail!(
                "dfy-codec: NV12 输入缓冲太小：需要 {need} 字节（stride={stride}, {w}x{h}），实得 {}",
                raw.len()
            );
        }
        for y in 0..h {
            let src = &raw[y * stride..y * stride + w];
            self.nv12_buf[y * w..y * w + w].copy_from_slice(src);
        }
        let uv_src_off = stride * h;
        let uv_dst_off = w * h;
        for y in 0..(h / 2) {
            let src = &raw[uv_src_off + y * stride..uv_src_off + y * stride + w];
            self.nv12_buf[uv_dst_off + y * w..uv_dst_off + y * w + w].copy_from_slice(src);
        }
        Ok(())
    }

    /// 由首个关键帧补齐 SPS/PPS（MFT 没给 sequence header 时）。
    fn fill_extra(&mut self, annexb: &[u8]) {
        if self.extra.sps.is_some() && self.extra.pps.is_some() {
            return;
        }
        let (sps, pps) = nal::extract_sps_pps(annexb);
        if let (Some(s), Some(p)) = (&sps, &pps) {
            self.extra.codec_private = Some(nal::build_avcc_config(s, p));
        }
        if sps.is_some() {
            self.extra.sps = sps;
        }
        if pps.is_some() {
            self.extra.pps = pps;
        }
    }

    /// 尝试用 Codec API 强制下一帧为关键帧；失败返回 false。
    fn force_keyframe(&mut self) -> bool {
        if let Some(api) = &self.codec_api {
            if set_codec_value(api, &CODECAPI_AVEncVideoForceKeyFrame, &variant_u32(1)) {
                return true;
            }
        }
        false
    }

    /// 构造一个输出缓冲描述（按 MFT 是否自己分配 sample 决定）。
    fn new_output_buffer(&self) -> Result<MFT_OUTPUT_DATA_BUFFER> {
        if self.provides_samples {
            Ok(MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(None),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            })
        } else {
            unsafe {
                let buffer = MFCreateMemoryBuffer(self.out_capacity)
                    .context("输出 MFCreateMemoryBuffer 失败")?;
                let sample = MFCreateSample().context("输出 MFCreateSample 失败")?;
                sample
                    .AddBuffer(&buffer)
                    .context("输出 IMFSample::AddBuffer 失败")?;
                Ok(MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: ManuallyDrop::new(Some(sample)),
                    dwStatus: 0,
                    pEvents: ManuallyDrop::new(None),
                })
            }
        }
    }

    /// 抽干编码器当前可用的输出。
    fn drain_outputs(&mut self) -> Result<Vec<EncodedFrame>> {
        let mut frames = Vec::new();
        loop {
            let mut dbuf = self.new_output_buffer()?;
            let mut status = 0u32;
            let res = unsafe {
                self.transform
                    .ProcessOutput(0, std::slice::from_mut(&mut dbuf), &mut status)
            };
            // ManuallyDrop 字段必须手动取走，否则 sample 引用泄漏。
            let sample = unsafe { ManuallyDrop::take(&mut dbuf.pSample) };
            let events = unsafe { ManuallyDrop::take(&mut dbuf.pEvents) };
            drop(events);

            match res {
                Ok(()) => {
                    let Some(sample) = sample else { break };
                    let mut data = copy_sample_data(&sample)?;
                    if data.is_empty() {
                        break;
                    }
                    // 个别 MFT 输出 AVCC，统一转 Annex-B。
                    if !nal::has_start_code(&data) {
                        if let Some(conv) = nal::avcc_to_annexb(&data) {
                            data = conv;
                        }
                    }
                    let is_keyframe = nal::is_keyframe(&data);
                    if is_keyframe {
                        self.fill_extra(&data);
                    }
                    let pts_us = self
                        .pending_pts
                        .pop_front()
                        .or_else(|| sample_time_us(&sample))
                        .unwrap_or(self.current_pts_us);
                    self.seen_output = true;
                    frames.push(EncodedFrame {
                        data,
                        is_keyframe,
                        pts_us,
                        extra: self.extra.clone(),
                    });
                }
                Err(e) => {
                    if debug_enabled() {
                        eprintln!(
                            "dfy-codec[debug]: 编码 ProcessOutput hresult=0x{:08x} status={status}",
                            e.code().0 as u32
                        );
                    }
                    if is_need_more_input(&e) || is_stream_change(&e) {
                        break;
                    }
                    return Err(e).context("ProcessOutput 失败");
                }
            }
            let _ = status;
        }
        Ok(frames)
    }
}

impl VideoEncoder for MfEncoder {
    fn encode(&mut self, raw: &[u8], stride: u32, pts_us: u64) -> Result<Vec<EncodedFrame>> {
        let fmt = self.resolve_format(raw, stride);
        match fmt {
            PixelFormatIn::Nv12 => self.copy_nv12(raw, stride)?,
            PixelFormatIn::Bgra8 => bgra_to_nv12_into(
                &mut self.nv12_buf,
                raw,
                stride,
                self.cfg.width,
                self.cfg.height,
            )?,
            PixelFormatIn::Rgba8 => rgba_to_nv12_into(
                &mut self.nv12_buf,
                raw,
                stride,
                self.cfg.width,
                self.cfg.height,
            )?,
        }
        self.current_pts_us = pts_us;

        if self.pending_keyframe {
            self.pending_keyframe = false;
            if !self.force_keyframe() {
                // Codec API 不可用：重建 transform，下一帧必为 IDR。
                self.rebuild().context("重建编码器以强制关键帧失败")?;
                self.force_keyframe();
            }
        }

        let sample = make_sample(
            &self.nv12_buf,
            (pts_us as i64).saturating_mul(10),
            self.frame_duration_100ns,
        )?;

        let mut frames = Vec::new();
        let mut fed = unsafe { self.transform.ProcessInput(0, &sample, 0).is_ok() };
        if !fed {
            // 输入队列满：先抽干输出再喂一次。
            frames.extend(self.drain_outputs()?);
            unsafe {
                self.transform
                    .ProcessInput(0, &sample, 0)
                    .context("ProcessInput（重试）失败")?;
            }
            fed = true;
        }
        if fed {
            self.pending_pts.push_back(pts_us);
        }
        if debug_enabled() {
            let out_status = unsafe { self.transform.GetOutputStatus() };
            eprintln!(
                "dfy-codec[debug]: 喂入第 {} 帧（fed={fed}），输出状态={out_status:?}",
                self.frame_count
            );
        }
        frames.extend(self.drain_outputs()?);
        self.frame_count += 1;
        Ok(frames)
    }

    fn request_keyframe(&mut self) {
        self.pending_keyframe = true;
    }

    fn set_bitrate(&mut self, kbps: u32) -> Result<()> {
        validate_bitrate_kbps(kbps)?;
        if kbps == self.cfg.bitrate_kbps {
            return Ok(());
        }
        let Some(api) = &self.codec_api else {
            bail!("dfy-codec: 当前编码 MFT 不支持 Codec API，无法动态改码率");
        };
        let bitrate = kbps
            .checked_mul(1000)
            .ok_or_else(|| anyhow!("dfy-codec: 码率换算溢出（{kbps} kbps）"))?;
        let mean_key = CODECAPI_AVEncCommonMeanBitRate;
        if unsafe { api.IsSupported(&mean_key) }.is_err() {
            bail!("dfy-codec: 编码 MFT 不支持 CODECAPI_AVEncCommonMeanBitRate，无法动态改码率");
        }
        unsafe { api.SetValue(&mean_key, &variant_u32(bitrate)) }
            .context("dfy-codec: 编码 MFT 设置 CODECAPI_AVEncCommonMeanBitRate 失败")?;
        // CBR/PeakVBR 的峰值约束也必须跟着调整；不支持峰值控制的 MFT仍可只用均值。
        let _ = set_codec_value(api, &CODECAPI_AVEncCommonMaxBitRate, &variant_u32(bitrate));
        self.cfg.bitrate_kbps = kbps;
        Ok(())
    }

    fn set_frame_rate(&mut self, fps: u32) -> Result<()> {
        validate_fps(fps)?;
        if fps == self.cfg.fps {
            return Ok(());
        }
        bail!(
            "dfy-codec: Media Foundation 编码 MFT 的帧率绑定到已协商媒体类型，不能在当前 transform 上从 {} 动态改为 {fps} fps；请用新 EncoderConfig 重建编码器",
            self.cfg.fps
        )
    }

    fn config(&self) -> Option<&EncoderConfig> {
        Some(&self.cfg)
    }

    fn flush(&mut self) -> Result<Vec<EncodedFrame>> {
        // COMMAND_DRAIN 让 MFT 把内部缓冲（微软软编约 16 帧）全部吐出，
        // 之后会以 MF_E_TRANSFORM_NEED_MORE_INPUT 结束。
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
        }
        let frames = self.drain_outputs()?;
        // 冲刷后 transform 需要重新开始流式处理才能继续编码。
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
        }
        self.pending_pts.clear();
        Ok(frames)
    }
}

// ---------------------------------------------------------------------------
// 解码器
// ---------------------------------------------------------------------------

/// Media Foundation H.264 解码器。
pub(crate) struct MfDecoder {
    _session: MfSession,
    transform: IMFTransform,
    input_type_set: bool,
    output_configured: bool,
    provides_samples: bool,
    out_capacity: u32,
    width: u32,
    height: u32,
    stride: u32,
    pts_counter_us: u64,
    frame_duration_100ns: i64,
    stream_change_count: u32,
    /// 是否已经把 SPS/PPS 作为 `MF_MT_MPEG_SEQUENCE_HEADER` 写进输入类型。
    seq_header_set: bool,
}

// SAFETY: 同 MfEncoder——对象只在 MTA 上使用，且结构体独占所有权。
unsafe impl Send for MfDecoder {}

impl MfDecoder {
    fn new(prefer_hardware: bool) -> Result<Self> {
        let session = MfSession::acquire()?;
        let candidates = sort_candidates(
            enumerate_mfts(
                MFT_CATEGORY_VIDEO_DECODER,
                MFVideoFormat_H264,
                MFVideoFormat_NV12,
            )
            .context("枚举 H.264 解码器失败")?,
            prefer_hardware,
        );
        if candidates.is_empty() {
            bail!("dfy-codec: 系统里没有注册 H.264 解码 MFT");
        }
        let only = std::env::var("DFY_CODEC_DEC_NAME").ok();
        for (idx, c) in candidates.iter().enumerate() {
            if debug_enabled() {
                eprintln!(
                    "dfy-codec[debug]: 解码候选 #{idx}「{}」硬件={}",
                    c.name, c.hardware
                );
            }
        }
        let mut errors = Vec::new();
        for cand in candidates {
            if let Some(ref only) = only {
                if !cand.name.contains(only.as_str()) {
                    continue;
                }
            }
            // 同编码器：激活前先允许异步 MFT（QSV/DXVA 解码器通常是异步的）。
            unsafe {
                let _ = cand.activate.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1);
            }
            let transform: IMFTransform = match unsafe { cand.activate.ActivateObject() } {
                Ok(t) => t,
                Err(e) => {
                    errors.push(format!("{}: 激活失败 {e}", cand.name));
                    continue;
                }
            };
            if let Ok(api) = transform.cast::<ICodecAPI>() {
                let _ = apply_codec_value(
                    &api,
                    &CODECAPI_AVLowLatencyMode,
                    &variant_u32(1),
                    "AVLowLatencyMode",
                );
            }
            if let Ok(attrs) = transform.cast::<IMFAttributes>() {
                unsafe {
                    let _ = attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1);
                }
                if debug_enabled() {
                    let async_ = unsafe { attrs.GetUINT32(&MF_TRANSFORM_ASYNC) }.unwrap_or(0);
                    let d3d = unsafe { attrs.GetUINT32(&MF_SA_D3D11_AWARE) }.unwrap_or(0);
                    eprintln!(
                        "dfy-codec[debug]: 解码 MFT「{}」MF_TRANSFORM_ASYNC={async_} D3D11_AWARE={d3d}",
                        cand.name
                    );
                }
            } else if debug_enabled() {
                eprintln!(
                    "dfy-codec[debug]: 解码 MFT「{}」不暴露 IMFAttributes",
                    cand.name
                );
            }
            let info =
                unsafe { transform.GetOutputStreamInfo(0) }.unwrap_or(MFT_OUTPUT_STREAM_INFO {
                    dwFlags: 0,
                    cbSize: 0,
                    cbAlignment: 0,
                });
            let provides_samples = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
            let mut dec = MfDecoder {
                _session: session.clone(),
                transform,
                input_type_set: false,
                output_configured: false,
                provides_samples,
                out_capacity: info.cbSize.max(2 * 1024 * 1024),
                width: 0,
                height: 0,
                stride: 0,
                pts_counter_us: 0,
                frame_duration_100ns: 10_000_000 / 30,
                stream_change_count: 0,
                seq_header_set: false,
            };
            // 解码器对「先设输入还是先设输出」的要求不一：先试先设输出类型。
            let out_first = dec.configure_output_type();
            if out_first.is_err() {
                // 退回到先设输入类型，再设输出类型。
                let in_type = match pick_decoder_input_type(&dec.transform) {
                    Ok(t) => t,
                    Err(e) => {
                        errors.push(format!("{}: 取输入类型失败 {e}", cand.name));
                        continue;
                    }
                };
                if let Err(e) = unsafe { dec.transform.SetInputType(0, &in_type, 0) } {
                    errors.push(format!("{}: SetInputType(H264) 失败 {e}", cand.name));
                    continue;
                }
                dec.input_type_set = true;
                let _ = dec.configure_output_type();
            } else {
                // 先设输出类型成功后，再设输入类型。
                let in_type = match pick_decoder_input_type(&dec.transform) {
                    Ok(t) => t,
                    Err(e) => {
                        errors.push(format!("{}: 取输入类型失败 {e}", cand.name));
                        continue;
                    }
                };
                if let Err(e) = unsafe { dec.transform.SetInputType(0, &in_type, 0) } {
                    // 输出先设不行就退回输入先设。
                    errors.push(format!("{}: 先输出后输入的顺序失败 {e}", cand.name));
                    continue;
                }
                dec.input_type_set = true;
            }
            if debug_enabled() {
                eprintln!(
                    "dfy-codec[debug]: 解码输出类型 NV12 状态={} width={} height={} stride={} provides_samples={}，输入类型已设={}",
                    if dec.output_configured { "已设置" } else { "未设置" },
                    dec.width,
                    dec.height,
                    dec.stride,
                    dec.provides_samples,
                    dec.input_type_set
                );
                let cur_in = unsafe { dec.transform.GetInputCurrentType(0) };
                match cur_in {
                    Ok(t) => match media_attrs(&t) {
                        Ok(a) => {
                            let sub = unsafe { a.GetGUID(&MF_MT_SUBTYPE) }
                                .map(|g| {
                                    format!("{{{:08X}-{:04X}-{:04X}}}", g.data1, g.data2, g.data3)
                                })
                                .unwrap_or_else(|e| format!("<读取失败 {e}>"));
                            let major = unsafe { a.GetGUID(&MF_MT_MAJOR_TYPE) }
                                .map(|g| {
                                    format!("{{{:08X}-{:04X}-{:04X}}}", g.data1, g.data2, g.data3)
                                })
                                .unwrap_or_else(|e| format!("<读取失败 {e}>"));
                            eprintln!(
                                "dfy-codec[debug]: 解码当前输入类型 major={major} subtype={sub}"
                            );
                        }
                        Err(e) => eprintln!("dfy-codec[debug]: 读输入类型属性失败：{e}"),
                    },
                    Err(e) => eprintln!("dfy-codec[debug]: GetInputCurrentType 失败：{e}"),
                }
            }
            // 类型都设好之后再通知开始流处理（顺序对部分 MFT 很关键）。
            unsafe {
                let _ = dec
                    .transform
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
                let _ = dec
                    .transform
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
            }
            return Ok(dec);
        }
        bail!(
            "dfy-codec: 所有 H.264 解码 MFT 初始化失败：\n  {}",
            errors.join("\n  ")
        )
    }

    /// 从可用输出类型里挑 NV12 并设置；同时记录尺寸/stride。
    fn configure_output_type(&mut self) -> Result<()> {
        let mut idx = 0u32;
        loop {
            let t = match unsafe { self.transform.GetOutputAvailableType(0, idx) } {
                Ok(t) => t,
                Err(e) => {
                    bail!("dfy-codec: 解码器没有 NV12 输出类型（已试 {idx} 个）: {e}");
                }
            };
            let a = media_attrs(&t)?;
            let sub = unsafe { a.GetGUID(&MF_MT_SUBTYPE) }.unwrap_or(GUID::zeroed());
            if sub == MFVideoFormat_NV12 {
                unsafe { self.transform.SetOutputType(0, &t, 0) }
                    .context("解码器 SetOutputType(NV12) 失败")?;
                let size = unsafe { a.GetUINT64(&MF_MT_FRAME_SIZE) }.unwrap_or(0);
                let (dw, dh) = ((size >> 32) as u32, size as u32);
                if dw > 0 && dh > 0 {
                    self.width = dw;
                    self.height = dh;
                }
                if let Ok(v) = unsafe { a.GetUINT32(&MF_MT_DEFAULT_STRIDE) } {
                    let stride = (v as i32).unsigned_abs();
                    if stride > 0 {
                        self.stride = stride;
                    }
                }
                if self.stride == 0 {
                    self.stride = self.width;
                }
                self.output_configured = true;
                let info = unsafe { self.transform.GetOutputStreamInfo(0) };
                if let Ok(info) = info {
                    self.provides_samples =
                        info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
                    self.out_capacity = info.cbSize.max(self.out_capacity);
                }
                return Ok(());
            }
            idx += 1;
        }
    }

    fn new_output_buffer(&self) -> Result<MFT_OUTPUT_DATA_BUFFER> {
        if self.provides_samples {
            Ok(MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(None),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            })
        } else {
            unsafe {
                let buffer = MFCreateMemoryBuffer(self.out_capacity)
                    .context("解码输出 MFCreateMemoryBuffer 失败")?;
                let sample = MFCreateSample().context("解码输出 MFCreateSample 失败")?;
                sample
                    .AddBuffer(&buffer)
                    .context("解码输出 AddBuffer 失败")?;
                Ok(MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: ManuallyDrop::new(Some(sample)),
                    dwStatus: 0,
                    pEvents: ManuallyDrop::new(None),
                })
            }
        }
    }

    /// 把解码 sample 变成 `DecodedFrame`。
    fn collect_frame(&mut self, sample: &IMFSample) -> Result<DecodedFrame> {
        let data = copy_sample_data(sample)?;
        if data.is_empty() {
            bail!("dfy-codec: 解码器输出空帧");
        }
        let mut stride = if self.stride > 0 {
            self.stride
        } else {
            self.width
        };
        let mut width = self.width;
        let mut height = self.height;
        if width == 0 || height == 0 {
            // 输出类型没给尺寸时，用 NV12 数据长度反推（data = stride*h*3/2）。
            if stride == 0 {
                bail!("dfy-codec: 解码器未报告帧尺寸，且 stride 未知");
            }
            height = (data.len() as u64 * 2 / (stride as u64 * 3)) as u32;
            width = stride;
        }
        if stride == 0 {
            stride = width;
        }
        let pts_us = sample_time_us(sample).unwrap_or(self.pts_counter_us);
        Ok(DecodedFrame {
            data,
            stride,
            width,
            height,
            format: PixelFormatOut::Nv12,
            pts_us,
        })
    }

    /// 抽干解码器当前可用输出；返回 (帧, 是否遇到 stream change)。
    fn drain_outputs(&mut self) -> Result<(Vec<DecodedFrame>, bool)> {
        let mut frames = Vec::new();
        loop {
            let mut dbuf = self.new_output_buffer()?;
            let mut status = 0u32;
            let res = unsafe {
                self.transform
                    .ProcessOutput(0, std::slice::from_mut(&mut dbuf), &mut status)
            };
            let sample = unsafe { ManuallyDrop::take(&mut dbuf.pSample) };
            let events = unsafe { ManuallyDrop::take(&mut dbuf.pEvents) };
            drop(events);
            match res {
                Ok(()) => {
                    let Some(sample) = sample else { break };
                    if let Ok(frame) = self.collect_frame(&sample) {
                        frames.push(frame);
                    }
                }
                Err(e) => {
                    if debug_enabled() {
                        eprintln!(
                            "dfy-codec[debug]: 解码 ProcessOutput hresult=0x{:08x} status={status}",
                            e.code().0 as u32
                        );
                    }
                    if is_need_more_input(&e) {
                        break;
                    }
                    if is_stream_change(&e) {
                        return Ok((frames, true));
                    }
                    return Err(e).context("解码 ProcessOutput 失败");
                }
            }
            let _ = status;
        }
        Ok((frames, false))
    }
}

impl VideoDecoder for MfDecoder {
    fn decode(&mut self, data: &[u8]) -> Result<Vec<DecodedFrame>> {
        if data.is_empty() {
            return Ok(Vec::new());
        }
        let _ = &self.input_type_set;
        // 统一喂 Annex-B；AVCC 先转换。
        let annexb = if nal::has_start_code(data) {
            data.to_vec()
        } else {
            match nal::avcc_to_annexb(data) {
                Some(v) => v,
                None => data.to_vec(),
            }
        };
        // 部分解码 MFT 只认输入类型里的 `MF_MT_MPEG_SEQUENCE_HEADER`（avcC），
        // 拿到首个 SPS/PPS 后补上，再重设一次输入类型。
        if !self.seq_header_set {
            let (sps, pps) = nal::extract_sps_pps(&annexb);
            if let (Some(sps), Some(pps)) = (sps, pps) {
                let cfg_bytes = nal::build_avcc_config(&sps, &pps);
                if let Ok(t) = unsafe { self.transform.GetInputCurrentType(0) } {
                    if let Ok(attrs) = t.cast::<IMFAttributes>() {
                        unsafe {
                            let _ = attrs.SetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &cfg_bytes);
                        }
                        match unsafe { self.transform.SetInputType(0, &t, 0) } {
                            Ok(()) => {
                                self.seq_header_set = true;
                                // 重设输入类型会把输出类型清掉，需要重新设置。
                                self.output_configured = false;
                                let _ = self.configure_output_type();
                                unsafe {
                                    let _ = self
                                        .transform
                                        .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
                                    let _ = self
                                        .transform
                                        .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
                                }
                                if debug_enabled() {
                                    eprintln!(
                                        "dfy-codec[debug]: 已写入序列头 avcC（{} 字节）并重设输入/输出类型",
                                        cfg_bytes.len()
                                    );
                                }
                            }
                            Err(e) => {
                                if debug_enabled() {
                                    eprintln!(
                                        "dfy-codec[debug]: 写入序列头后 SetInputType 失败：{e}"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
        // MF 解码 MFT 要求一次喂一个访问单元，整条码流进来时按 AUD 切开。
        let units = nal::annexb_access_units(&annexb);
        if debug_enabled() {
            eprintln!(
                "dfy-codec[debug]: 解码输入 {} 字节（Annex-B {} 字节），切出 {} 个访问单元",
                data.len(),
                annexb.len(),
                units.len()
            );
        }
        let mut frames = Vec::new();
        for (start, end) in units {
            let unit = &annexb[start..end];
            if unit.is_empty() {
                continue;
            }
            let got = self.decode_access_unit(unit)?;
            if debug_enabled() {
                eprintln!(
                    "dfy-codec[debug]: 访问单元 {start}..{end}（{} 字节）→ {} 帧",
                    unit.len(),
                    got.len()
                );
            }
            frames.extend(got);
        }
        Ok(frames)
    }

    fn flush(&mut self) -> Result<Vec<DecodedFrame>> {
        // 微软 H.264 解码 MFT 会攒十几帧甚至二十几帧才吐第一帧（重排序/前瞻缓冲），
        // 只有 COMMAND_DRAIN 会把它内部缓冲全部冲刷出来。
        unsafe { self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0) }
            .context("解码器发送 COMMAND_DRAIN 失败")?;
        let mut frames = Vec::new();
        loop {
            let (drained, changed) = self.drain_outputs()?;
            let n = drained.len();
            frames.extend(drained);
            if changed {
                self.output_configured = false;
                self.configure_output_type()?;
                continue;
            }
            if n == 0 {
                break;
            }
        }
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            // 冲刷后若还要继续解码，需要重新开始流。
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
        }
        if debug_enabled() {
            eprintln!("dfy-codec[debug]: 解码冲刷出 {} 帧", frames.len());
        }
        Ok(frames)
    }
}

impl MfDecoder {
    /// 解码单个访问单元（Annex-B）。
    fn decode_access_unit(&mut self, annexb: &[u8]) -> Result<Vec<DecodedFrame>> {
        let pts_us = self.pts_counter_us;
        self.pts_counter_us += (self.frame_duration_100ns / 10) as u64;
        let sample = make_sample(
            annexb,
            (pts_us as i64).saturating_mul(10),
            self.frame_duration_100ns,
        )?;

        let mut frames = Vec::new();
        let mut fed = false;
        let mut attempts = 0u32;
        loop {
            attempts += 1;
            if attempts > 64 {
                bail!("dfy-codec: 解码循环次数超限（可能是 MFT 卡住）");
            }
            if !fed {
                match unsafe { self.transform.ProcessInput(0, &sample, 0) } {
                    Ok(()) => fed = true,
                    Err(e) => {
                        // 输入队列满：先抽输出，再重试一次。
                        let (drained, changed) = self.drain_outputs()?;
                        frames.extend(drained);
                        if changed {
                            continue;
                        }
                        if let Err(e2) = unsafe { self.transform.ProcessInput(0, &sample, 0) } {
                            if debug_enabled() {
                                eprintln!(
                                    "dfy-codec[debug]: 解码 ProcessInput 失败 hresult=0x{:08x}",
                                    e2.code().0 as u32
                                );
                            }
                            return Err(e2)
                                .context(format!("解码 ProcessInput 失败（首次错误：{e}）"));
                        }
                        fed = true;
                    }
                }
            }
            let (drained, changed) = self.drain_outputs()?;
            frames.extend(drained);
            if debug_enabled() && attempts <= 2 {
                let in_status = unsafe { self.transform.GetInputStatus(0) };
                let out_status = unsafe { self.transform.GetOutputStatus() };
                eprintln!(
                    "dfy-codec[debug]: 解码状态 in={in_status:?} out={out_status:?} 本单元已出 {} 帧",
                    frames.len()
                );
            }
            if debug_enabled() && frames.is_empty() && attempts <= 2 {
                if let Ok(gen) = self.transform.cast::<IMFMediaEventGenerator>() {
                    for _ in 0..4 {
                        match unsafe { gen.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                            Ok(ev) => {
                                let ty = unsafe { ev.GetType() }.unwrap_or(0);
                                eprintln!(
                                    "dfy-codec[debug]: 解码事件 type=0x{ty:08x}（METransformNeedInput=0x{need:08x} METransformHaveOutput=0x{have:08x}）",
                                    need = METransformNeedInput.0,
                                    have = METransformHaveOutput.0
                                );
                            }
                            Err(e) => {
                                eprintln!("dfy-codec[debug]: 解码事件队列空（{e}）");
                                break;
                            }
                        }
                    }
                }
            }
            if changed {
                self.stream_change_count += 1;
                if self.stream_change_count > 8 {
                    bail!("dfy-codec: 解码器反复要求重配输出类型（>8 次）");
                }
                self.output_configured = false;
                self.configure_output_type()
                    .context("解码器重配输出类型失败")?;
                continue;
            }
            break;
        }
        Ok(frames)
    }
}

/// 从 MFT 声明的输入类型里挑 H.264；拿不到就自己造一个。
fn pick_decoder_input_type(transform: &IMFTransform) -> Result<IMFMediaType> {
    let mut idx = 0u32;
    while let Ok(t) = unsafe { transform.GetInputAvailableType(0, idx) } {
        if let Ok(a) = media_attrs(&t) {
            if let Ok(sub) = unsafe { a.GetGUID(&MF_MT_SUBTYPE) } {
                if sub == MFVideoFormat_H264 {
                    return Ok(t);
                }
            }
        }
        idx += 1;
    }
    build_decoder_input_type()
}

// ---------------------------------------------------------------------------
// 工厂函数（由 lib.rs 调用）
// ---------------------------------------------------------------------------

pub(crate) fn create_encoder(
    cfg: &EncoderConfig,
    format: Option<PixelFormatIn>,
) -> Result<Box<dyn VideoEncoder>> {
    // 4 字节/像素格式但显式指定 NV12 之类的组合是不合法的，提前拦掉。
    if let Some(PixelFormatIn::Nv12) = format {
        if !cfg.width.is_multiple_of(2) || !cfg.height.is_multiple_of(2) {
            bail!("dfy-codec: NV12 输入要求宽高为偶数");
        }
    }
    Ok(Box::new(MfEncoder::new(cfg, format)?))
}

pub(crate) fn create_decoder(prefer_hardware: bool) -> Result<Box<dyn VideoDecoder>> {
    Ok(Box::new(MfDecoder::new(prefer_hardware)?))
}

pub(crate) fn available_encoders() -> Vec<EncoderInfo> {
    let Ok(_session) = MfSession::acquire() else {
        eprintln!("dfy-codec: MFStartup 失败，无法枚举编码器");
        return Vec::new();
    };
    let list = match enumerate_mfts(
        MFT_CATEGORY_VIDEO_ENCODER,
        MFVideoFormat_NV12,
        MFVideoFormat_H264,
    ) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("dfy-codec: 枚举编码器失败：{e:#}");
            return Vec::new();
        }
    };
    list.into_iter()
        .map(|c| {
            let available = unsafe { c.activate.ActivateObject::<IMFTransform>() }.is_ok();
            EncoderInfo {
                name: c.name,
                codec: Codec::H264,
                hardware: c.hardware,
                available,
            }
        })
        .collect()
}
