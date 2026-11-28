// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! Apple VideoToolbox H.264 硬件优先编解码后端（macOS）。
//!
//! 编码输出统一为 Annex-B（关键帧附带 SPS/PPS）；解码器同时接受 Annex-B 与 AVCC，
//! 输出紧凑 NV12（`stride == width`）供上层颜色转换使用。

#![allow(non_upper_case_globals)]

use std::ffi::{c_char, c_int, c_void, CStr};
use std::ptr::{self, NonNull};
use std::sync::Mutex;

use anyhow::{anyhow, bail, ensure, Context, Result};

use crate::format::{bgra_to_nv12_into, nv12_len, rgba_to_nv12_into};
use crate::nal::{self, BitstreamKind};
use crate::{
    validate_bitrate_kbps, validate_fps, Codec, CodecExtra, DecodedFrame, EncodedFrame,
    EncoderConfig, EncoderInfo, PixelFormatIn, PixelFormatOut, VideoDecoder, VideoEncoder,
};

type OSStatus = i32;
type CVReturn = i32;
type CFIndex = isize;
type CFTypeID = usize;
type CFTypeRef = *const c_void;
type CFAllocatorRef = *const c_void;
type CFStringRef = *const c_void;
type CFBooleanRef = *const c_void;
type CFNumberRef = *const c_void;
type CFArrayRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFMutableDictionaryRef = *mut c_void;
type VTCompressionSessionRef = *mut c_void;
type VTDecompressionSessionRef = *mut c_void;
type CVPixelBufferRef = *mut c_void;
type CVImageBufferRef = *mut c_void;
type CMFormatDescriptionRef = *mut c_void;
type CMVideoFormatDescriptionRef = *mut c_void;
type CMBlockBufferRef = *mut c_void;
type CMSampleBufferRef = *mut c_void;
type VTEncodeInfoFlags = u32;
type VTDecodeFrameFlags = u32;
type VTDecodeInfoFlags = u32;

const K_CF_NUMBER_SINT32_TYPE: CFIndex = 3;
const K_CF_NUMBER_FLOAT64_TYPE: CFIndex = 6;
const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const K_CV_PIXEL_BUFFER_LOCK_READ_ONLY: u64 = 0x0000_0001;
const K_CM_BLOCK_BUFFER_ASSURE_MEMORY_NOW_FLAG: u32 = 1 << 0;

const K_CM_VIDEO_CODEC_TYPE_H264: u32 = u32::from_be_bytes(*b"avc1");
const K_CV_PIXEL_FORMAT_420V: u32 = u32::from_be_bytes(*b"420v");

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

impl CMTime {
    const fn invalid() -> Self {
        Self {
            value: 0,
            timescale: 0,
            flags: 0,
            epoch: 0,
        }
    }

    const fn make_us(pts_us: u64) -> Self {
        Self {
            value: pts_us as i64,
            timescale: 1_000_000,
            flags: 1, // kCMTimeFlags_Valid
            epoch: 0,
        }
    }

    fn to_us(self) -> Option<u64> {
        if (self.flags & 1) == 0 || self.timescale <= 0 || self.value < 0 {
            return None;
        }
        let us = (self.value as i128) * 1_000_000 / (self.timescale as i128);
        u64::try_from(us).ok()
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CMSampleTimingInfo {
    duration: CMTime,
    presentation_time_stamp: CMTime,
    decode_time_stamp: CMTime,
}

type VTCompressionOutputCallback = extern "C" fn(
    output_callback_ref_con: *mut c_void,
    source_frame_ref_con: *mut c_void,
    status: OSStatus,
    info_flags: VTEncodeInfoFlags,
    sample_buffer: CMSampleBufferRef,
);

type VTDecompressionOutputCallback = extern "C" fn(
    decompression_output_ref_con: *mut c_void,
    source_frame_ref_con: *mut c_void,
    status: OSStatus,
    info_flags: VTDecodeInfoFlags,
    image_buffer: CVImageBufferRef,
    presentation_time_stamp: CMTime,
    presentation_duration: CMTime,
);

#[repr(C)]
struct VTDecompressionOutputCallbackRecord {
    decompression_output_callback: Option<VTDecompressionOutputCallback>,
    decompression_output_ref_con: *mut c_void,
}

#[allow(clippy::duplicated_attributes)]
#[link(name = "VideoToolbox", kind = "framework")]
#[link(name = "CoreMedia", kind = "framework")]
#[link(name = "CoreVideo", kind = "framework")]
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFAllocatorDefault: CFAllocatorRef;
    static kCFBooleanTrue: CFBooleanRef;
    static kCFBooleanFalse: CFBooleanRef;
    static kCFTypeDictionaryKeyCallBacks: u8;
    static kCFTypeDictionaryValueCallBacks: u8;

    fn CFRelease(cf: CFTypeRef);
    fn CFGetTypeID(cf: CFTypeRef) -> CFTypeID;
    fn CFBooleanGetTypeID() -> CFTypeID;
    fn CFBooleanGetValue(boolean: CFBooleanRef) -> u8;
    fn CFNumberCreate(
        allocator: CFAllocatorRef,
        the_type: CFIndex,
        value_ptr: *const c_void,
    ) -> CFNumberRef;
    fn CFNumberGetValue(number: CFNumberRef, the_type: CFIndex, value_ptr: *mut c_void) -> u8;
    fn CFArrayGetCount(the_array: CFArrayRef) -> CFIndex;
    fn CFArrayGetValueAtIndex(the_array: CFArrayRef, idx: CFIndex) -> *const c_void;
    fn CFDictionaryCreateMutable(
        allocator: CFAllocatorRef,
        capacity: CFIndex,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFMutableDictionaryRef;
    fn CFDictionarySetValue(
        the_dict: CFMutableDictionaryRef,
        key: *const c_void,
        value: *const c_void,
    );
    fn CFDictionaryGetValueIfPresent(
        the_dict: CFDictionaryRef,
        key: *const c_void,
        value: *mut *const c_void,
    ) -> u8;
    fn CFStringGetCString(
        the_string: CFStringRef,
        buffer: *mut c_char,
        buffer_size: CFIndex,
        encoding: u32,
    ) -> u8;

    static kCVPixelBufferPixelFormatTypeKey: CFStringRef;
    fn CVPixelBufferCreate(
        allocator: CFAllocatorRef,
        width: usize,
        height: usize,
        pixel_format_type: u32,
        pixel_buffer_attributes: CFDictionaryRef,
        pixel_buffer_out: *mut CVPixelBufferRef,
    ) -> CVReturn;
    fn CVPixelBufferLockBaseAddress(pixel_buffer: CVPixelBufferRef, lock_flags: u64) -> CVReturn;
    fn CVPixelBufferUnlockBaseAddress(
        pixel_buffer: CVPixelBufferRef,
        unlock_flags: u64,
    ) -> CVReturn;
    fn CVPixelBufferGetWidth(pixel_buffer: CVPixelBufferRef) -> usize;
    fn CVPixelBufferGetHeight(pixel_buffer: CVPixelBufferRef) -> usize;
    fn CVPixelBufferGetBaseAddressOfPlane(
        pixel_buffer: CVPixelBufferRef,
        plane_index: usize,
    ) -> *mut c_void;
    fn CVPixelBufferGetBytesPerRowOfPlane(
        pixel_buffer: CVPixelBufferRef,
        plane_index: usize,
    ) -> usize;

    static kCMSampleAttachmentKey_NotSync: CFStringRef;
    fn CMVideoFormatDescriptionCreateFromH264ParameterSets(
        allocator: CFAllocatorRef,
        parameter_set_count: usize,
        parameter_set_pointers: *const *const u8,
        parameter_set_sizes: *const usize,
        nal_unit_header_length: c_int,
        format_description_out: *mut CMFormatDescriptionRef,
    ) -> OSStatus;
    fn CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
        video_desc: CMFormatDescriptionRef,
        parameter_set_index: usize,
        parameter_set_pointer_out: *mut *const u8,
        parameter_set_size_out: *mut usize,
        parameter_set_count_out: *mut usize,
        nal_unit_header_length_out: *mut c_int,
    ) -> OSStatus;
    fn CMBlockBufferCreateWithMemoryBlock(
        structure_allocator: CFAllocatorRef,
        memory_block: *mut c_void,
        block_length: usize,
        block_allocator: CFAllocatorRef,
        custom_block_source: *const c_void,
        offset_to_data: usize,
        data_length: usize,
        flags: u32,
        block_buffer_out: *mut CMBlockBufferRef,
    ) -> OSStatus;
    fn CMBlockBufferReplaceDataBytes(
        source_bytes: *const c_void,
        destination_buffer: CMBlockBufferRef,
        offset_into_destination: usize,
        data_length: usize,
    ) -> OSStatus;
    fn CMBlockBufferGetDataLength(the_buffer: CMBlockBufferRef) -> usize;
    fn CMBlockBufferCopyDataBytes(
        the_source_buffer: CMBlockBufferRef,
        offset_to_data: usize,
        data_length: usize,
        destination: *mut c_void,
    ) -> OSStatus;
    fn CMSampleBufferCreateReady(
        allocator: CFAllocatorRef,
        data_buffer: CMBlockBufferRef,
        format_description: CMFormatDescriptionRef,
        num_samples: CFIndex,
        num_sample_timing_entries: CFIndex,
        sample_timing_array: *const CMSampleTimingInfo,
        num_sample_size_entries: CFIndex,
        sample_size_array: *const usize,
        sample_buffer_out: *mut CMSampleBufferRef,
    ) -> OSStatus;
    fn CMSampleBufferGetDataBuffer(sbuf: CMSampleBufferRef) -> CMBlockBufferRef;
    fn CMSampleBufferGetFormatDescription(sbuf: CMSampleBufferRef) -> CMFormatDescriptionRef;
    fn CMSampleBufferGetOutputPresentationTimeStamp(sbuf: CMSampleBufferRef) -> CMTime;
    fn CMSampleBufferGetSampleAttachmentsArray(
        sbuf: CMSampleBufferRef,
        create_if_necessary: u8,
    ) -> CFArrayRef;

    static kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder: CFStringRef;
    static kVTCompressionPropertyKey_RealTime: CFStringRef;
    static kVTCompressionPropertyKey_AllowFrameReordering: CFStringRef;
    static kVTCompressionPropertyKey_ProfileLevel: CFStringRef;
    static kVTProfileLevel_H264_Main_AutoLevel: CFStringRef;
    static kVTCompressionPropertyKey_ExpectedFrameRate: CFStringRef;
    static kVTCompressionPropertyKey_MaxKeyFrameInterval: CFStringRef;
    static kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration: CFStringRef;
    static kVTCompressionPropertyKey_AverageBitRate: CFStringRef;
    static kVTEncodeFrameOptionKey_ForceKeyFrame: CFStringRef;

    static kVTVideoEncoderList_CodecType: CFStringRef;
    static kVTVideoEncoderList_EncoderName: CFStringRef;
    static kVTVideoEncoderList_DisplayName: CFStringRef;
    static kVTVideoEncoderList_IsHardwareAccelerated: CFStringRef;

    fn VTSessionSetProperty(
        session: CFTypeRef,
        property_key: CFStringRef,
        property_value: CFTypeRef,
    ) -> OSStatus;
    fn VTCompressionSessionCreate(
        allocator: CFAllocatorRef,
        width: i32,
        height: i32,
        codec_type: u32,
        encoder_specification: CFDictionaryRef,
        source_image_buffer_attributes: CFDictionaryRef,
        compressed_data_allocator: CFAllocatorRef,
        output_callback: Option<VTCompressionOutputCallback>,
        output_callback_ref_con: *mut c_void,
        compression_session_out: *mut VTCompressionSessionRef,
    ) -> OSStatus;
    fn VTCompressionSessionPrepareToEncodeFrames(session: VTCompressionSessionRef) -> OSStatus;
    fn VTCompressionSessionEncodeFrame(
        session: VTCompressionSessionRef,
        image_buffer: CVImageBufferRef,
        presentation_time_stamp: CMTime,
        duration: CMTime,
        frame_properties: CFDictionaryRef,
        source_frame_ref_con: *mut c_void,
        info_flags_out: *mut VTEncodeInfoFlags,
    ) -> OSStatus;
    fn VTCompressionSessionCompleteFrames(
        session: VTCompressionSessionRef,
        complete_until_presentation_time_stamp: CMTime,
    ) -> OSStatus;
    fn VTCompressionSessionInvalidate(session: VTCompressionSessionRef);

    fn VTDecompressionSessionCreate(
        allocator: CFAllocatorRef,
        video_format_description: CMVideoFormatDescriptionRef,
        video_decoder_specification: CFDictionaryRef,
        destination_image_buffer_attributes: CFDictionaryRef,
        output_callback: *const VTDecompressionOutputCallbackRecord,
        decompression_session_out: *mut VTDecompressionSessionRef,
    ) -> OSStatus;
    fn VTDecompressionSessionDecodeFrame(
        session: VTDecompressionSessionRef,
        sample_buffer: CMSampleBufferRef,
        decode_flags: VTDecodeFrameFlags,
        source_frame_ref_con: *mut c_void,
        info_flags_out: *mut VTDecodeInfoFlags,
    ) -> OSStatus;
    fn VTDecompressionSessionWaitForAsynchronousFrames(
        session: VTDecompressionSessionRef,
    ) -> OSStatus;
    fn VTDecompressionSessionInvalidate(session: VTDecompressionSessionRef);

    fn VTCopyVideoEncoderList(
        options: CFDictionaryRef,
        list_of_video_encoders_out: *mut CFArrayRef,
    ) -> OSStatus;
}

struct CfRef(NonNull<c_void>);

impl CfRef {
    unsafe fn from_raw_retained(ptr: *const c_void) -> Option<Self> {
        NonNull::new(ptr as *mut c_void).map(Self)
    }

    fn as_ptr(&self) -> *const c_void {
        self.0.as_ptr()
    }
}

impl Drop for CfRef {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0.as_ptr()) };
    }
}

unsafe fn create_cf_dict() -> Result<CFMutableDictionaryRef> {
    let dict = unsafe {
        CFDictionaryCreateMutable(
            kCFAllocatorDefault,
            0,
            &kCFTypeDictionaryKeyCallBacks as *const u8 as *const c_void,
            &kCFTypeDictionaryValueCallBacks as *const u8 as *const c_void,
        )
    };
    if dict.is_null() {
        bail!("dfy-codec: CFDictionaryCreateMutable 失败");
    }
    Ok(dict)
}

unsafe fn cf_number_i32(val: i32) -> Result<CfRef> {
    let num = unsafe {
        CFNumberCreate(
            kCFAllocatorDefault,
            K_CF_NUMBER_SINT32_TYPE,
            &val as *const i32 as *const c_void,
        )
    };
    unsafe { CfRef::from_raw_retained(num) }.context("CFNumberCreate(i32) 失败")
}

unsafe fn cf_number_f64(val: f64) -> Result<CfRef> {
    let num = unsafe {
        CFNumberCreate(
            kCFAllocatorDefault,
            K_CF_NUMBER_FLOAT64_TYPE,
            &val as *const f64 as *const c_void,
        )
    };
    unsafe { CfRef::from_raw_retained(num) }.context("CFNumberCreate(f64) 失败")
}

unsafe fn cf_string_to_rust(s: CFStringRef) -> Option<String> {
    if s.is_null() {
        return None;
    }
    let mut buf = [0 as c_char; 512];
    let ok = unsafe {
        CFStringGetCString(
            s,
            buf.as_mut_ptr(),
            buf.len() as CFIndex,
            K_CF_STRING_ENCODING_UTF8,
        )
    };
    if ok == 0 {
        return None;
    }
    let cstr = unsafe { CStr::from_ptr(buf.as_ptr()) };
    Some(cstr.to_string_lossy().into_owned())
}

unsafe fn set_session_prop(session: *const c_void, key: CFStringRef, val: CFTypeRef) -> OSStatus {
    unsafe { VTSessionSetProperty(session, key, val) }
}

unsafe fn pixel_buffer_attrs_420v() -> Result<CfRef> {
    let dict = unsafe { create_cf_dict()? };
    let dict_guard = unsafe { CfRef::from_raw_retained(dict) }.context("dict wrap")?;
    let fmt = unsafe { cf_number_i32(K_CV_PIXEL_FORMAT_420V as i32)? };
    unsafe {
        CFDictionarySetValue(dict, kCVPixelBufferPixelFormatTypeKey, fmt.as_ptr());
    }
    Ok(dict_guard)
}

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

struct EncoderCallbackState {
    frames: Vec<EncodedFrame>,
    extra: CodecExtra,
    last_error: Option<String>,
}

struct VtH264Encoder {
    session: VTCompressionSessionRef,
    state: *mut Mutex<EncoderCallbackState>,
    cfg: EncoderConfig,
    input_format: Option<PixelFormatIn>,
    nv12_buf: Vec<u8>,
    pending_keyframe: bool,
    last_pts_us: u64,
}

unsafe impl Send for VtH264Encoder {}

fn avcc_to_annexb_with_len(avcc: &[u8], nal_len_bytes: usize) -> Option<Vec<u8>> {
    if !matches!(nal_len_bytes, 1 | 2 | 4) {
        return None;
    }
    let mut out = Vec::with_capacity(avcc.len() + 16);
    let mut i = 0usize;
    while i + nal_len_bytes <= avcc.len() {
        let len = match nal_len_bytes {
            1 => avcc[i] as usize,
            2 => u16::from_be_bytes([avcc[i], avcc[i + 1]]) as usize,
            4 => u32::from_be_bytes([avcc[i], avcc[i + 1], avcc[i + 2], avcc[i + 3]]) as usize,
            _ => unreachable!(),
        };
        i += nal_len_bytes;
        if len == 0 || i + len > avcc.len() {
            return None;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&avcc[i..i + len]);
        i += len;
    }
    if i == avcc.len() && !out.is_empty() {
        Some(out)
    } else {
        None
    }
}

extern "C" fn compression_output_callback(
    output_callback_ref_con: *mut c_void,
    source_frame_ref_con: *mut c_void,
    status: OSStatus,
    _info_flags: VTEncodeInfoFlags,
    sample_buffer: CMSampleBufferRef,
) {
    if output_callback_ref_con.is_null() {
        return;
    }
    let state_mutex = unsafe { &*(output_callback_ref_con as *const Mutex<EncoderCallbackState>) };
    let mut state = state_mutex.lock().unwrap_or_else(|e| e.into_inner());
    if status != 0 {
        state.last_error = Some(format!("VideoToolbox 编码回调错误 OSStatus={status}"));
        return;
    }
    if sample_buffer.is_null() {
        return;
    }

    let mut is_keyframe = true;
    unsafe {
        let attachments = CMSampleBufferGetSampleAttachmentsArray(sample_buffer, 0);
        if !attachments.is_null() && CFArrayGetCount(attachments) > 0 {
            let dict = CFArrayGetValueAtIndex(attachments, 0) as CFDictionaryRef;
            if !dict.is_null() {
                let mut not_sync: *const c_void = ptr::null();
                if CFDictionaryGetValueIfPresent(
                    dict,
                    kCMSampleAttachmentKey_NotSync,
                    &mut not_sync,
                ) != 0
                    && !not_sync.is_null()
                    && CFGetTypeID(not_sync) == CFBooleanGetTypeID()
                {
                    is_keyframe = CFBooleanGetValue(not_sync as CFBooleanRef) == 0;
                }
            }
        }
    }

    let mut nal_header_len = 4usize;
    let mut sps_opt: Option<Vec<u8>> = None;
    let mut pps_opt: Option<Vec<u8>> = None;
    unsafe {
        let fmt_desc = CMSampleBufferGetFormatDescription(sample_buffer);
        if !fmt_desc.is_null() {
            let mut param_count = 0usize;
            let mut hdr_len: c_int = 4;
            let mut p0: *const u8 = ptr::null();
            let mut s0 = 0usize;
            if CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                fmt_desc,
                0,
                &mut p0,
                &mut s0,
                &mut param_count,
                &mut hdr_len,
            ) == 0
                && !p0.is_null()
                && s0 > 0
            {
                sps_opt = Some(std::slice::from_raw_parts(p0, s0).to_vec());
                if matches!(hdr_len, 1 | 2 | 4) {
                    nal_header_len = hdr_len as usize;
                }
            }
            if param_count > 1 {
                let mut p1: *const u8 = ptr::null();
                let mut s1 = 0usize;
                if CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                    fmt_desc,
                    1,
                    &mut p1,
                    &mut s1,
                    ptr::null_mut(),
                    ptr::null_mut(),
                ) == 0
                    && !p1.is_null()
                    && s1 > 0
                {
                    pps_opt = Some(std::slice::from_raw_parts(p1, s1).to_vec());
                }
            }
        }
    }

    if let (Some(sps), Some(pps)) = (&sps_opt, &pps_opt) {
        if state.extra.sps.is_none() || state.extra.pps.is_none() {
            state.extra.sps = Some(sps.clone());
            state.extra.pps = Some(pps.clone());
            state.extra.codec_private = Some(nal::build_avcc_config(sps, pps));
        }
    }

    let block_buf = unsafe { CMSampleBufferGetDataBuffer(sample_buffer) };
    if block_buf.is_null() {
        return;
    }
    let total_len = unsafe { CMBlockBufferGetDataLength(block_buf) };
    if total_len == 0 {
        return;
    }
    let mut raw = vec![0u8; total_len];
    let copy_rc = unsafe {
        CMBlockBufferCopyDataBytes(block_buf, 0, total_len, raw.as_mut_ptr() as *mut c_void)
    };
    if copy_rc != 0 {
        state.last_error = Some(format!(
            "CMBlockBufferCopyDataBytes 失败 OSStatus={copy_rc}"
        ));
        return;
    }

    let Some(mut annexb) = avcc_to_annexb_with_len(&raw, nal_header_len) else {
        state.last_error = Some("VideoToolbox 输出 AVCC 切分失败".to_string());
        return;
    };

    if is_keyframe {
        let (has_sps, has_pps) = {
            let (s, p) = nal::extract_sps_pps(&annexb);
            (s.is_some(), p.is_some())
        };
        if !has_sps || !has_pps {
            let sps = sps_opt.as_ref().or(state.extra.sps.as_ref());
            let pps = pps_opt.as_ref().or(state.extra.pps.as_ref());
            if let (Some(sps), Some(pps)) = (sps, pps) {
                let mut prefixed = Vec::with_capacity(8 + sps.len() + pps.len() + annexb.len());
                if !has_sps {
                    prefixed.extend_from_slice(&[0, 0, 0, 1]);
                    prefixed.extend_from_slice(sps);
                }
                if !has_pps {
                    prefixed.extend_from_slice(&[0, 0, 0, 1]);
                    prefixed.extend_from_slice(pps);
                }
                prefixed.extend_from_slice(&annexb);
                annexb = prefixed;
            }
        }
    }

    let pts_us = unsafe { CMSampleBufferGetOutputPresentationTimeStamp(sample_buffer) }
        .to_us()
        .unwrap_or(source_frame_ref_con as usize as u64);

    let extra = state.extra.clone();
    state.frames.push(EncodedFrame {
        data: annexb,
        is_keyframe,
        pts_us,
        extra,
    });
}

impl VtH264Encoder {
    fn new(cfg: &EncoderConfig, input_format: Option<PixelFormatIn>) -> Result<Self> {
        let nv12_size = nv12_len(cfg.width, cfg.height)
            .ok_or_else(|| anyhow!("dfy-codec: 分辨率溢出 {}x{}", cfg.width, cfg.height))?;

        let state = Box::into_raw(Box::new(Mutex::new(EncoderCallbackState {
            frames: Vec::new(),
            extra: CodecExtra::default(),
            last_error: None,
        })));

        let session = match Self::create_session(cfg, state) {
            Ok(session) => session,
            Err(err) => {
                unsafe { drop(Box::from_raw(state)) };
                return Err(err);
            }
        };

        Ok(Self {
            session,
            state,
            cfg: cfg.clone(),
            input_format,
            nv12_buf: vec![0u8; nv12_size],
            pending_keyframe: false,
            last_pts_us: 0,
        })
    }

    fn create_session(
        cfg: &EncoderConfig,
        state: *mut Mutex<EncoderCallbackState>,
    ) -> Result<VTCompressionSessionRef> {
        let enc_spec = unsafe {
            let dict = create_cf_dict()?;
            let guard = CfRef::from_raw_retained(dict).context("spec wrap")?;
            let flag = if cfg.prefer_hardware {
                kCFBooleanTrue
            } else {
                kCFBooleanFalse
            };
            CFDictionarySetValue(
                dict,
                kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
                flag,
            );
            guard
        };
        let src_attrs = unsafe { pixel_buffer_attrs_420v()? };

        let mut session: VTCompressionSessionRef = ptr::null_mut();
        let mut status = unsafe {
            VTCompressionSessionCreate(
                kCFAllocatorDefault,
                cfg.width as i32,
                cfg.height as i32,
                K_CM_VIDEO_CODEC_TYPE_H264,
                enc_spec.as_ptr(),
                src_attrs.as_ptr(),
                kCFAllocatorDefault,
                Some(compression_output_callback),
                state as *mut c_void,
                &mut session,
            )
        };
        if status != 0 && !cfg.prefer_hardware {
            status = unsafe {
                VTCompressionSessionCreate(
                    kCFAllocatorDefault,
                    cfg.width as i32,
                    cfg.height as i32,
                    K_CM_VIDEO_CODEC_TYPE_H264,
                    ptr::null(),
                    src_attrs.as_ptr(),
                    kCFAllocatorDefault,
                    Some(compression_output_callback),
                    state as *mut c_void,
                    &mut session,
                )
            };
        }
        if status != 0 || session.is_null() {
            bail!("dfy-codec: VTCompressionSessionCreate 失败 OSStatus={status}");
        }

        if let Err(err) = unsafe { Self::configure_session(session, cfg) } {
            unsafe {
                VTCompressionSessionInvalidate(session);
                CFRelease(session);
            }
            return Err(err);
        }
        Ok(session)
    }

    unsafe fn configure_session(
        session: VTCompressionSessionRef,
        cfg: &EncoderConfig,
    ) -> Result<()> {
        let _ = unsafe {
            set_session_prop(session, kVTCompressionPropertyKey_RealTime, kCFBooleanTrue)
        };
        let _ = unsafe {
            set_session_prop(
                session,
                kVTCompressionPropertyKey_AllowFrameReordering,
                kCFBooleanFalse,
            )
        };
        let _ = unsafe {
            set_session_prop(
                session,
                kVTCompressionPropertyKey_ProfileLevel,
                kVTProfileLevel_H264_Main_AutoLevel,
            )
        };

        let fps_num = unsafe { cf_number_f64(f64::from(cfg.fps))? };
        let _ = unsafe {
            set_session_prop(
                session,
                kVTCompressionPropertyKey_ExpectedFrameRate,
                fps_num.as_ptr(),
            )
        };

        let gop_num = unsafe { cf_number_i32(cfg.keyframe_interval.min(i32::MAX as u32) as i32)? };
        let _ = unsafe {
            set_session_prop(
                session,
                kVTCompressionPropertyKey_MaxKeyFrameInterval,
                gop_num.as_ptr(),
            )
        };

        let gop_dur = f64::from(cfg.keyframe_interval) / f64::from(cfg.fps.max(1));
        let gop_dur_num = unsafe { cf_number_f64(gop_dur)? };
        let _ = unsafe {
            set_session_prop(
                session,
                kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
                gop_dur_num.as_ptr(),
            )
        };

        let bps = cfg.bitrate_kbps.saturating_mul(1000).min(i32::MAX as u32) as i32;
        let bps_num = unsafe { cf_number_i32(bps)? };
        let rc = unsafe {
            set_session_prop(
                session,
                kVTCompressionPropertyKey_AverageBitRate,
                bps_num.as_ptr(),
            )
        };
        ensure!(
            rc == 0,
            "dfy-codec: 设置 VideoToolbox 码率失败 OSStatus={rc}"
        );

        let prep = unsafe { VTCompressionSessionPrepareToEncodeFrames(session) };
        ensure!(
            prep == 0,
            "dfy-codec: VTCompressionSessionPrepareToEncodeFrames 失败 OSStatus={prep}"
        );
        Ok(())
    }

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

    fn copy_nv12(&mut self, raw: &[u8], stride: u32) -> Result<()> {
        let w = self.cfg.width as usize;
        let h = self.cfg.height as usize;
        let stride = stride as usize;
        ensure!(stride >= w, "dfy-codec: NV12 stride {stride} 小于宽度 {w}");
        let need = stride
            .checked_mul(h + h / 2)
            .ok_or_else(|| anyhow!("dfy-codec: NV12 尺寸溢出"))?;
        ensure!(
            raw.len() >= need,
            "dfy-codec: NV12 输入缓冲太小：需要 {need} 字节，实得 {}",
            raw.len()
        );
        for y in 0..h {
            self.nv12_buf[y * w..y * w + w].copy_from_slice(&raw[y * stride..y * stride + w]);
        }
        let uv_src = stride * h;
        let uv_dst = w * h;
        for y in 0..(h / 2) {
            self.nv12_buf[uv_dst + y * w..uv_dst + y * w + w]
                .copy_from_slice(&raw[uv_src + y * stride..uv_src + y * stride + w]);
        }
        Ok(())
    }

    fn make_pixel_buffer(&self) -> Result<CfRef> {
        let mut pb: CVPixelBufferRef = ptr::null_mut();
        let rc = unsafe {
            CVPixelBufferCreate(
                kCFAllocatorDefault,
                self.cfg.width as usize,
                self.cfg.height as usize,
                K_CV_PIXEL_FORMAT_420V,
                ptr::null(),
                &mut pb,
            )
        };
        ensure!(
            rc == 0 && !pb.is_null(),
            "dfy-codec: CVPixelBufferCreate 失败 CVReturn={rc}"
        );
        let guard = unsafe { CfRef::from_raw_retained(pb) }.context("CVPixelBuffer wrap")?;

        let lock_rc = unsafe { CVPixelBufferLockBaseAddress(pb, 0) };
        ensure!(
            lock_rc == 0,
            "CVPixelBufferLockBaseAddress 失败 CVReturn={lock_rc}"
        );

        let w = self.cfg.width as usize;
        let h = self.cfg.height as usize;
        unsafe {
            let y_dst = CVPixelBufferGetBaseAddressOfPlane(pb, 0) as *mut u8;
            let y_stride = CVPixelBufferGetBytesPerRowOfPlane(pb, 0);
            let uv_dst = CVPixelBufferGetBaseAddressOfPlane(pb, 1) as *mut u8;
            let uv_stride = CVPixelBufferGetBytesPerRowOfPlane(pb, 1);

            if !y_dst.is_null() && y_stride >= w {
                for row in 0..h {
                    ptr::copy_nonoverlapping(
                        self.nv12_buf[row * w..].as_ptr(),
                        y_dst.add(row * y_stride),
                        w,
                    );
                }
            }
            let uv_off = w * h;
            if !uv_dst.is_null() && uv_stride >= w {
                for row in 0..(h / 2) {
                    ptr::copy_nonoverlapping(
                        self.nv12_buf[uv_off + row * w..].as_ptr(),
                        uv_dst.add(row * uv_stride),
                        w,
                    );
                }
            }
            let _ = CVPixelBufferUnlockBaseAddress(pb, 0);
        }
        Ok(guard)
    }

    fn take_frames(&self) -> Result<Vec<EncodedFrame>> {
        let state = unsafe { &*self.state };
        let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(err) = guard.last_error.take() {
            bail!("{err}");
        }
        Ok(std::mem::take(&mut guard.frames))
    }
}

impl Drop for VtH264Encoder {
    fn drop(&mut self) {
        if !self.session.is_null() {
            unsafe {
                let _ = VTCompressionSessionCompleteFrames(self.session, CMTime::invalid());
                VTCompressionSessionInvalidate(self.session);
                CFRelease(self.session);
            }
            self.session = ptr::null_mut();
        }
        if !self.state.is_null() {
            unsafe { drop(Box::from_raw(self.state)) };
            self.state = ptr::null_mut();
        }
    }
}

impl VideoEncoder for VtH264Encoder {
    fn encode(&mut self, raw: &[u8], stride: u32, pts_us: u64) -> Result<Vec<EncodedFrame>> {
        match self.resolve_format(raw, stride) {
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

        let pb = self.make_pixel_buffer()?;
        let force_dict = if self.pending_keyframe {
            self.pending_keyframe = false;
            unsafe {
                let dict = create_cf_dict()?;
                let guard = CfRef::from_raw_retained(dict).context("force dict wrap")?;
                CFDictionarySetValue(dict, kVTEncodeFrameOptionKey_ForceKeyFrame, kCFBooleanTrue);
                Some(guard)
            }
        } else {
            None
        };

        let pts = CMTime::make_us(pts_us);
        let dur = CMTime::make_us(self.cfg.frame_duration_us().max(1));
        self.last_pts_us = pts_us;

        let rc = unsafe {
            VTCompressionSessionEncodeFrame(
                self.session,
                pb.as_ptr() as CVImageBufferRef,
                pts,
                dur,
                force_dict.as_ref().map_or(ptr::null(), |d| d.as_ptr()),
                pts_us as usize as *mut c_void,
                ptr::null_mut(),
            )
        };
        ensure!(
            rc == 0,
            "dfy-codec: VTCompressionSessionEncodeFrame 失败 OSStatus={rc}"
        );

        let complete_rc = unsafe { VTCompressionSessionCompleteFrames(self.session, pts) };
        ensure!(
            complete_rc == 0,
            "dfy-codec: VTCompressionSessionCompleteFrames 失败 OSStatus={complete_rc}"
        );
        self.take_frames()
    }

    fn set_bitrate(&mut self, kbps: u32) -> Result<()> {
        validate_bitrate_kbps(kbps)?;
        if kbps == self.cfg.bitrate_kbps {
            return Ok(());
        }
        let bps = kbps.saturating_mul(1000).min(i32::MAX as u32) as i32;
        let bps_num = unsafe { cf_number_i32(bps)? };
        let rc = unsafe {
            set_session_prop(
                self.session,
                kVTCompressionPropertyKey_AverageBitRate,
                bps_num.as_ptr(),
            )
        };
        ensure!(
            rc == 0,
            "dfy-codec: VideoToolbox 动态设置码率失败 OSStatus={rc}"
        );
        self.cfg.bitrate_kbps = kbps;
        Ok(())
    }

    fn set_frame_rate(&mut self, fps: u32) -> Result<()> {
        validate_fps(fps)?;
        if fps == self.cfg.fps {
            return Ok(());
        }
        let fps_num = unsafe { cf_number_f64(f64::from(fps))? };
        let rc = unsafe {
            set_session_prop(
                self.session,
                kVTCompressionPropertyKey_ExpectedFrameRate,
                fps_num.as_ptr(),
            )
        };
        ensure!(
            rc == 0,
            "dfy-codec: VideoToolbox 动态设置帧率失败 OSStatus={rc}"
        );
        self.cfg.fps = fps;
        Ok(())
    }

    fn config(&self) -> Option<&EncoderConfig> {
        Some(&self.cfg)
    }

    fn request_keyframe(&mut self) {
        self.pending_keyframe = true;
    }

    fn flush(&mut self) -> Result<Vec<EncodedFrame>> {
        let rc = unsafe { VTCompressionSessionCompleteFrames(self.session, CMTime::invalid()) };
        ensure!(rc == 0, "dfy-codec: VideoToolbox flush 失败 OSStatus={rc}");
        self.take_frames()
    }
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

struct DecoderCallbackState {
    frames: Vec<DecodedFrame>,
    next_pts_us: u64,
    last_error: Option<String>,
}

struct VtH264Decoder {
    session: VTDecompressionSessionRef,
    format_desc: CMVideoFormatDescriptionRef,
    state: *mut Mutex<DecoderCallbackState>,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    params_dirty: bool,
    frame_index: u64,
}

unsafe impl Send for VtH264Decoder {}

extern "C" fn decompression_output_callback(
    decompression_output_ref_con: *mut c_void,
    _source_frame_ref_con: *mut c_void,
    status: OSStatus,
    _info_flags: VTDecodeInfoFlags,
    image_buffer: CVImageBufferRef,
    presentation_time_stamp: CMTime,
    _presentation_duration: CMTime,
) {
    if decompression_output_ref_con.is_null() {
        return;
    }
    let state_mutex =
        unsafe { &*(decompression_output_ref_con as *const Mutex<DecoderCallbackState>) };
    let mut state = state_mutex.lock().unwrap_or_else(|e| e.into_inner());
    if status != 0 {
        state.last_error = Some(format!("VideoToolbox 解码回调错误 OSStatus={status}"));
        return;
    }
    if image_buffer.is_null() {
        return;
    }

    let lock_rc =
        unsafe { CVPixelBufferLockBaseAddress(image_buffer, K_CV_PIXEL_BUFFER_LOCK_READ_ONLY) };
    if lock_rc != 0 {
        state.last_error = Some(format!(
            "CVPixelBufferLockBaseAddress 失败 CVReturn={lock_rc}"
        ));
        return;
    }

    let w = unsafe { CVPixelBufferGetWidth(image_buffer) } as u32;
    let h = unsafe { CVPixelBufferGetHeight(image_buffer) } as u32;
    if w == 0 || h == 0 {
        unsafe {
            let _ = CVPixelBufferUnlockBaseAddress(image_buffer, K_CV_PIXEL_BUFFER_LOCK_READ_ONLY);
        };
        return;
    }

    let w_usize = w as usize;
    let h_usize = h as usize;
    let mut nv12 = vec![0u8; w_usize * h_usize + w_usize * (h_usize / 2)];
    unsafe {
        let y_ptr = CVPixelBufferGetBaseAddressOfPlane(image_buffer, 0) as *const u8;
        let y_stride = CVPixelBufferGetBytesPerRowOfPlane(image_buffer, 0);
        let uv_ptr = CVPixelBufferGetBaseAddressOfPlane(image_buffer, 1) as *const u8;
        let uv_stride = CVPixelBufferGetBytesPerRowOfPlane(image_buffer, 1);

        if !y_ptr.is_null() && y_stride >= w_usize {
            for row in 0..h_usize {
                ptr::copy_nonoverlapping(
                    y_ptr.add(row * y_stride),
                    nv12[row * w_usize..].as_mut_ptr(),
                    w_usize,
                );
            }
        }
        let uv_off = w_usize * h_usize;
        if !uv_ptr.is_null() && uv_stride >= w_usize {
            for row in 0..(h_usize / 2) {
                ptr::copy_nonoverlapping(
                    uv_ptr.add(row * uv_stride),
                    nv12[uv_off + row * w_usize..].as_mut_ptr(),
                    w_usize,
                );
            }
        }
        let _ = CVPixelBufferUnlockBaseAddress(image_buffer, K_CV_PIXEL_BUFFER_LOCK_READ_ONLY);
    }

    let pts_us = presentation_time_stamp.to_us().unwrap_or(state.next_pts_us);
    state.next_pts_us = pts_us.saturating_add(33_333);
    state.frames.push(DecodedFrame {
        data: nv12,
        stride: w,
        width: w,
        height: h,
        format: PixelFormatOut::Nv12,
        pts_us,
    });
}

impl VtH264Decoder {
    fn new(_prefer_hardware: bool) -> Result<Self> {
        let state = Box::into_raw(Box::new(Mutex::new(DecoderCallbackState {
            frames: Vec::new(),
            next_pts_us: 0,
            last_error: None,
        })));
        Ok(Self {
            session: ptr::null_mut(),
            format_desc: ptr::null_mut(),
            state,
            sps: None,
            pps: None,
            params_dirty: false,
            frame_index: 0,
        })
    }

    fn teardown_session(&mut self) {
        if !self.session.is_null() {
            unsafe {
                let _ = VTDecompressionSessionWaitForAsynchronousFrames(self.session);
                VTDecompressionSessionInvalidate(self.session);
                CFRelease(self.session);
            }
            self.session = ptr::null_mut();
        }
        if !self.format_desc.is_null() {
            unsafe { CFRelease(self.format_desc) };
            self.format_desc = ptr::null_mut();
        }
    }

    fn ensure_session(&mut self) -> Result<bool> {
        if self.sps.is_none() || self.pps.is_none() {
            return Ok(false);
        }
        if !self.session.is_null() && !self.params_dirty {
            return Ok(true);
        }

        self.teardown_session();

        let sps = self.sps.as_ref().expect("checked");
        let pps = self.pps.as_ref().expect("checked");
        let ptrs = [sps.as_ptr(), pps.as_ptr()];
        let sizes = [sps.len(), pps.len()];
        let mut fmt_desc: CMFormatDescriptionRef = ptr::null_mut();
        let rc = unsafe {
            CMVideoFormatDescriptionCreateFromH264ParameterSets(
                kCFAllocatorDefault,
                2,
                ptrs.as_ptr(),
                sizes.as_ptr(),
                4,
                &mut fmt_desc,
            )
        };
        ensure!(
            rc == 0 && !fmt_desc.is_null(),
            "dfy-codec: CMVideoFormatDescriptionCreateFromH264ParameterSets 失败 OSStatus={rc}"
        );

        let dst_attrs = match unsafe { pixel_buffer_attrs_420v() } {
            Ok(attrs) => attrs,
            Err(err) => {
                unsafe { CFRelease(fmt_desc) };
                return Err(err);
            }
        };

        let cb_rec = VTDecompressionOutputCallbackRecord {
            decompression_output_callback: Some(decompression_output_callback),
            decompression_output_ref_con: self.state as *mut c_void,
        };
        let mut session: VTDecompressionSessionRef = ptr::null_mut();
        let dec_rc = unsafe {
            VTDecompressionSessionCreate(
                kCFAllocatorDefault,
                fmt_desc,
                ptr::null(),
                dst_attrs.as_ptr(),
                &cb_rec,
                &mut session,
            )
        };
        if dec_rc != 0 || session.is_null() {
            unsafe { CFRelease(fmt_desc) };
            bail!("dfy-codec: VTDecompressionSessionCreate 失败 OSStatus={dec_rc}");
        }

        self.format_desc = fmt_desc;
        self.session = session;
        self.params_dirty = false;
        Ok(true)
    }

    fn decode_access_unit(&mut self, au_nals: &[&[u8]]) -> Result<()> {
        if au_nals.is_empty() {
            return Ok(());
        }
        if !self.ensure_session()? {
            return Ok(());
        }

        let total_len: usize = au_nals.iter().map(|n| 4 + n.len()).sum();
        let mut avcc = Vec::with_capacity(total_len);
        for nal_slice in au_nals {
            let len = u32::try_from(nal_slice.len()).context("NAL 长度超出 u32")?;
            avcc.extend_from_slice(&len.to_be_bytes());
            avcc.extend_from_slice(nal_slice);
        }

        let mut block_buf: CMBlockBufferRef = ptr::null_mut();
        let bb_rc = unsafe {
            CMBlockBufferCreateWithMemoryBlock(
                kCFAllocatorDefault,
                ptr::null_mut(),
                avcc.len(),
                kCFAllocatorDefault,
                ptr::null(),
                0,
                avcc.len(),
                K_CM_BLOCK_BUFFER_ASSURE_MEMORY_NOW_FLAG,
                &mut block_buf,
            )
        };
        ensure!(
            bb_rc == 0 && !block_buf.is_null(),
            "CMBlockBufferCreateWithMemoryBlock 失败 OSStatus={bb_rc}"
        );
        let bb_guard =
            unsafe { CfRef::from_raw_retained(block_buf) }.context("CMBlockBuffer wrap")?;

        let rep_rc = unsafe {
            CMBlockBufferReplaceDataBytes(avcc.as_ptr() as *const c_void, block_buf, 0, avcc.len())
        };
        ensure!(
            rep_rc == 0,
            "CMBlockBufferReplaceDataBytes 失败 OSStatus={rep_rc}"
        );

        let pts = CMTime::make_us(self.frame_index.saturating_mul(33_333));
        self.frame_index = self.frame_index.saturating_add(1);
        let timing = CMSampleTimingInfo {
            duration: CMTime::make_us(33_333),
            presentation_time_stamp: pts,
            decode_time_stamp: CMTime::invalid(),
        };
        let sample_size = avcc.len();
        let mut sample_buf: CMSampleBufferRef = ptr::null_mut();
        let sb_rc = unsafe {
            CMSampleBufferCreateReady(
                kCFAllocatorDefault,
                bb_guard.as_ptr() as CMBlockBufferRef,
                self.format_desc,
                1,
                1,
                &timing,
                1,
                &sample_size,
                &mut sample_buf,
            )
        };
        ensure!(
            sb_rc == 0 && !sample_buf.is_null(),
            "CMSampleBufferCreateReady 失败 OSStatus={sb_rc}"
        );
        let sb_guard =
            unsafe { CfRef::from_raw_retained(sample_buf) }.context("CMSampleBuffer wrap")?;

        let dec_rc = unsafe {
            VTDecompressionSessionDecodeFrame(
                self.session,
                sb_guard.as_ptr() as CMSampleBufferRef,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        ensure!(
            dec_rc == 0,
            "dfy-codec: VTDecompressionSessionDecodeFrame 失败 OSStatus={dec_rc}"
        );
        Ok(())
    }

    fn take_frames(&self) -> Result<Vec<DecodedFrame>> {
        let state = unsafe { &*self.state };
        let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(err) = guard.last_error.take() {
            bail!("{err}");
        }
        Ok(std::mem::take(&mut guard.frames))
    }
}

impl Drop for VtH264Decoder {
    fn drop(&mut self) {
        self.teardown_session();
        if !self.state.is_null() {
            unsafe { drop(Box::from_raw(self.state)) };
            self.state = ptr::null_mut();
        }
    }
}

impl VideoDecoder for VtH264Decoder {
    fn decode(&mut self, data: &[u8]) -> Result<Vec<DecodedFrame>> {
        if data.is_empty() {
            return Ok(Vec::new());
        }
        let nals = match nal::detect_kind(data) {
            Some(BitstreamKind::AnnexB) => nal::annexb_nals(data),
            Some(BitstreamKind::Avcc) => nal::avcc_nals(data).unwrap_or_default(),
            None => bail!("dfy-codec: 无法识别的 H.264 码流（非 Annex-B 也非 AVCC）"),
        };

        let mut au_nals: Vec<&[u8]> = Vec::new();
        let mut au_has_vcl = false;

        for range in nals {
            let nal_slice = &data[range.start..range.end];
            if nal_slice.is_empty() {
                continue;
            }
            let nal_type = nal_slice[0] & 0x1f;
            match nal_type {
                7 => {
                    if au_has_vcl {
                        let pending = std::mem::take(&mut au_nals);
                        au_has_vcl = false;
                        self.decode_access_unit(&pending)?;
                    }
                    if self.sps.as_deref() != Some(nal_slice) {
                        self.sps = Some(nal_slice.to_vec());
                        self.params_dirty = true;
                    }
                }
                8 => {
                    if au_has_vcl {
                        let pending = std::mem::take(&mut au_nals);
                        au_has_vcl = false;
                        self.decode_access_unit(&pending)?;
                    }
                    if self.pps.as_deref() != Some(nal_slice) {
                        self.pps = Some(nal_slice.to_vec());
                        self.params_dirty = true;
                    }
                }
                9 => {
                    if au_has_vcl {
                        let pending = std::mem::take(&mut au_nals);
                        au_has_vcl = false;
                        self.decode_access_unit(&pending)?;
                    }
                }
                6 => {
                    if au_has_vcl {
                        let pending = std::mem::take(&mut au_nals);
                        au_has_vcl = false;
                        self.decode_access_unit(&pending)?;
                    }
                    au_nals.push(nal_slice);
                }
                1..=5 => {
                    let first_slice_of_pic = nal_slice.len() >= 2 && (nal_slice[1] & 0x80) != 0;
                    if au_has_vcl && first_slice_of_pic {
                        let pending = std::mem::take(&mut au_nals);
                        self.decode_access_unit(&pending)?;
                    }
                    au_nals.push(nal_slice);
                    au_has_vcl = true;
                }
                _ => {
                    au_nals.push(nal_slice);
                }
            }
        }

        if au_has_vcl {
            self.decode_access_unit(&au_nals)?;
        }

        self.take_frames()
    }

    fn flush(&mut self) -> Result<Vec<DecodedFrame>> {
        if !self.session.is_null() {
            let rc = unsafe { VTDecompressionSessionWaitForAsynchronousFrames(self.session) };
            ensure!(
                rc == 0,
                "dfy-codec: VTDecompressionSessionWaitForAsynchronousFrames 失败 OSStatus={rc}"
            );
        }
        self.take_frames()
    }
}

// ---------------------------------------------------------------------------
// Backend factory & encoder enumeration
// ---------------------------------------------------------------------------

pub(crate) fn create_encoder(
    cfg: &EncoderConfig,
    format: Option<PixelFormatIn>,
) -> Result<Box<dyn VideoEncoder>> {
    Ok(Box::new(VtH264Encoder::new(cfg, format)?))
}

pub(crate) fn create_decoder(prefer_hardware: bool) -> Result<Box<dyn VideoDecoder>> {
    Ok(Box::new(VtH264Decoder::new(prefer_hardware)?))
}

pub(crate) fn available_encoders() -> Vec<EncoderInfo> {
    let mut out = Vec::new();
    let mut list_ref: CFArrayRef = ptr::null();
    let rc = unsafe { VTCopyVideoEncoderList(ptr::null(), &mut list_ref) };
    if rc != 0 || list_ref.is_null() {
        return out;
    }
    let Some(_list_guard) = (unsafe { CfRef::from_raw_retained(list_ref) }) else {
        return out;
    };

    let count = unsafe { CFArrayGetCount(list_ref) };
    for i in 0..count {
        let dict = unsafe { CFArrayGetValueAtIndex(list_ref, i) } as CFDictionaryRef;
        if dict.is_null() {
            continue;
        }
        let mut codec_val: *const c_void = ptr::null();
        if unsafe {
            CFDictionaryGetValueIfPresent(dict, kVTVideoEncoderList_CodecType, &mut codec_val)
        } == 0
            || codec_val.is_null()
        {
            continue;
        }
        let mut codec_fourcc: i32 = 0;
        if unsafe {
            CFNumberGetValue(
                codec_val as CFNumberRef,
                K_CF_NUMBER_SINT32_TYPE,
                &mut codec_fourcc as *mut i32 as *mut c_void,
            )
        } == 0
            || codec_fourcc as u32 != K_CM_VIDEO_CODEC_TYPE_H264
        {
            continue;
        }

        let mut name_val: *const c_void = ptr::null();
        let has_name = unsafe {
            CFDictionaryGetValueIfPresent(dict, kVTVideoEncoderList_EncoderName, &mut name_val) != 0
                || CFDictionaryGetValueIfPresent(
                    dict,
                    kVTVideoEncoderList_DisplayName,
                    &mut name_val,
                ) != 0
        };
        let name = if has_name {
            unsafe { cf_string_to_rust(name_val) }
        } else {
            None
        }
        .unwrap_or_else(|| "Apple VideoToolbox H.264".to_string());

        let mut hw_val: *const c_void = ptr::null();
        let hardware = unsafe {
            CFDictionaryGetValueIfPresent(
                dict,
                kVTVideoEncoderList_IsHardwareAccelerated,
                &mut hw_val,
            ) != 0
                && !hw_val.is_null()
                && CFGetTypeID(hw_val) == CFBooleanGetTypeID()
                && CFBooleanGetValue(hw_val as CFBooleanRef) != 0
        };

        out.push(EncoderInfo {
            name,
            codec: Codec::H264,
            hardware,
            available: true,
        });
    }
    out
}
