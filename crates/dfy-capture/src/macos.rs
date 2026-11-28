// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! macOS `ScreenCaptureKit` desktop capture backend.

use std::ffi::{c_char, c_void, CStr};

use anyhow::anyhow;

use crate::{CaptureError, CapturedFrame, MonitorInfo, PixelFormat};

const MAX_MONITORS: usize = 32;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DfyMacMonitor {
    display_id: u32,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    primary: u32,
    dpi: u32,
}

#[repr(C)]
struct DfySckFrame {
    width: u32,
    height: u32,
    stride: u32,
    seq: u64,
    pts_ns: i64,
    data: *mut u8,
    data_len: usize,
}

impl Default for DfySckFrame {
    fn default() -> Self {
        Self {
            width: 0,
            height: 0,
            stride: 0,
            seq: 0,
            pts_ns: 0,
            data: std::ptr::null_mut(),
            data_len: 0,
        }
    }
}

extern "C" {
    fn dfy_sck_preflight_permission() -> i32;
    fn dfy_sck_enumerate_monitors(out: *mut DfyMacMonitor, cap: u32, out_count: *mut u32) -> i32;
    fn dfy_sck_create(
        display_id: u32,
        width: u32,
        height: u32,
        max_fps: u32,
        err_buf: *mut c_char,
        err_cap: usize,
    ) -> *mut c_void;
    fn dfy_sck_capture(
        handle: *mut c_void,
        timeout_ms: u32,
        last_seq_inout: *mut u64,
        out_frame: *mut DfySckFrame,
        err_buf: *mut c_char,
        err_cap: usize,
    ) -> i32;
    fn dfy_sck_free_frame(ptr: *mut u8);
    fn dfy_sck_destroy(handle: *mut c_void);
}

fn c_err_to_string(buf: &[c_char]) -> String {
    unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

/// Captures frames from a single macOS display using `ScreenCaptureKit`.
pub struct MacCapturer {
    handle: *mut c_void,
    monitor: MonitorInfo,
    last_seq: u64,
}

// `MacCapturer` owns an isolated Obj-C `DfySckStream` whose internal frame buffer is
// protected by a pthread mutex; intending single capture-thread ownership like DXGI/Linux.

impl MacCapturer {
    /// Returns whether macOS Screen Recording TCC permission is currently granted.
    #[must_use]
    pub fn has_screen_recording_permission() -> bool {
        unsafe { dfy_sck_preflight_permission() != 0 }
    }

    /// Enumerates active macOS displays via CoreGraphics (`CGGetActiveDisplayList`).
    pub fn monitors() -> Result<Vec<MonitorInfo>, CaptureError> {
        let mut raw = [DfyMacMonitor::default(); MAX_MONITORS];
        let mut count = 0u32;
        let rc = unsafe {
            dfy_sck_enumerate_monitors(raw.as_mut_ptr(), MAX_MONITORS as u32, &mut count)
        };
        if rc != 0 {
            return Err(CaptureError::Other(anyhow!(
                "CGGetActiveDisplayList 失败（CGError={rc}）"
            )));
        }
        let mut out = Vec::with_capacity(count as usize);
        for (idx, item) in raw[..count as usize].iter().enumerate() {
            out.push(MonitorInfo {
                id: idx as u32,
                name: format!("Display {} (CG#{})", idx + 1, item.display_id),
                x: item.x,
                y: item.y,
                w: item.w,
                h: item.h,
                primary: item.primary != 0,
                dpi: item.dpi,
            });
        }
        // Keep primary display as index 0 (CGGetActiveDisplayList already puts main first, but sort
        // defensively in case a multi-GPU setup reorders it).
        out.sort_by_key(|m| !m.primary);
        for (idx, m) in out.iter_mut().enumerate() {
            m.id = idx as u32;
        }
        Ok(out)
    }

    /// Starts capturing the display at `monitor_index` (from [`Self::monitors`]).
    pub fn new(monitor_index: u32) -> Result<Self, CaptureError> {
        let mut raw = [DfyMacMonitor::default(); MAX_MONITORS];
        let mut count = 0u32;
        let rc = unsafe {
            dfy_sck_enumerate_monitors(raw.as_mut_ptr(), MAX_MONITORS as u32, &mut count)
        };
        if rc != 0 {
            return Err(CaptureError::Other(anyhow!(
                "CGGetActiveDisplayList 失败（CGError={rc}）"
            )));
        }
        let mut entries: Vec<(DfyMacMonitor, MonitorInfo)> = raw[..count as usize]
            .iter()
            .enumerate()
            .map(|(idx, item)| {
                (
                    *item,
                    MonitorInfo {
                        id: idx as u32,
                        name: format!("Display {} (CG#{})", idx + 1, item.display_id),
                        x: item.x,
                        y: item.y,
                        w: item.w,
                        h: item.h,
                        primary: item.primary != 0,
                        dpi: item.dpi,
                    },
                )
            })
            .collect();
        entries.sort_by_key(|(_, info)| !info.primary);
        for (idx, (_, info)) in entries.iter_mut().enumerate() {
            info.id = idx as u32;
        }

        let Some((raw_mon, monitor)) = entries.get(monitor_index as usize).cloned() else {
            return Err(CaptureError::NoSuchMonitor(monitor_index));
        };
        if monitor.w < 2 || monitor.h < 2 {
            return Err(CaptureError::Other(anyhow!(
                "显示器 {} 尺寸异常：{}x{}",
                monitor_index,
                monitor.w,
                monitor.h
            )));
        }

        let width = (monitor.w & !1).max(2);
        let height = (monitor.h & !1).max(2);
        let mut err_buf = [0 as c_char; 512];
        let handle = unsafe {
            dfy_sck_create(
                raw_mon.display_id,
                width,
                height,
                120,
                err_buf.as_mut_ptr(),
                err_buf.len(),
            )
        };
        if handle.is_null() {
            let msg = c_err_to_string(&err_buf);
            return Err(CaptureError::Other(anyhow!(
                "启动 macOS ScreenCaptureKit 采集失败：{msg}"
            )));
        }

        Ok(Self {
            handle,
            monitor,
            last_seq: 0,
        })
    }

    /// Returns metadata for the display being captured.
    #[must_use]
    pub fn monitor(&self) -> &MonitorInfo {
        &self.monitor
    }

    /// Waits up to `timeout_ms` for a newly captured BGRA8 frame.
    ///
    /// Returns `Ok(None)` when no new frame completed before the timeout.
    pub fn capture(&mut self, timeout_ms: u32) -> Result<Option<CapturedFrame>, CaptureError> {
        if self.handle.is_null() {
            return Err(CaptureError::Other(anyhow!(
                "ScreenCaptureKit 采集器已释放"
            )));
        }
        let mut out = DfySckFrame::default();
        let mut err_buf = [0 as c_char; 512];
        let rc = unsafe {
            dfy_sck_capture(
                self.handle,
                timeout_ms,
                &mut self.last_seq,
                &mut out,
                err_buf.as_mut_ptr(),
                err_buf.len(),
            )
        };
        if rc < 0 {
            let msg = c_err_to_string(&err_buf);
            return Err(CaptureError::Other(anyhow!(
                "ScreenCaptureKit 读取屏幕帧失败：{msg}"
            )));
        }
        if rc == 0 {
            return Ok(None);
        }
        let data = if out.data.is_null() || out.data_len == 0 {
            Vec::new()
        } else {
            // SAFETY: dfy_sck_capture allocated `out.data` of `out.data_len` bytes via malloc and
            // transferred ownership; we copy into Vec<u8> and immediately free via dfy_sck_free_frame.
            let slice = unsafe { std::slice::from_raw_parts(out.data, out.data_len) };
            let vec = slice.to_vec();
            unsafe { dfy_sck_free_frame(out.data) };
            vec
        };
        Ok(Some(CapturedFrame {
            width: out.width,
            height: out.height,
            stride: out.stride,
            format: PixelFormat::Bgra8,
            data,
            timestamp_qpc: out.pts_ns,
            dirty: Vec::new(),
            full_frame: true,
        }))
    }

    /// Stops the underlying `SCStream` and releases Objective-C resources (idempotent).
    pub fn release(&mut self) {
        if !self.handle.is_null() {
            unsafe { dfy_sck_destroy(self.handle) };
            self.handle = std::ptr::null_mut();
        }
    }
}

impl Drop for MacCapturer {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitors_returns_valid_geometry() {
        let monitors = MacCapturer::monitors().expect("enumerate macOS displays");
        if monitors.is_empty() {
            eprintln!("skip: no active macOS displays found");
            return;
        }
        assert!(
            monitors[0].primary,
            "primary monitor should be sorted first"
        );
        for (idx, m) in monitors.iter().enumerate() {
            assert_eq!(m.id, idx as u32);
            assert!(m.w >= 2 && m.h >= 2, "invalid dimensions: {m:?}");
            println!(
                "macOS display[{}]: {} {}x{} @({},{}) primary={} dpi={}",
                m.id, m.name, m.w, m.h, m.x, m.y, m.primary, m.dpi
            );
        }
    }

    #[test]
    fn out_of_range_monitor_returns_no_such_monitor() {
        let err = match MacCapturer::new(999) {
            Ok(_) => panic!("index 999 should fail"),
            Err(err) => err,
        };
        assert!(matches!(err, CaptureError::NoSuchMonitor(999)));
    }

    #[test]
    fn missing_permission_reports_clear_error_or_constructs() {
        if !MacCapturer::has_screen_recording_permission() {
            let err = match MacCapturer::new(0) {
                Ok(_) => panic!("unpermitted capture must fail"),
                Err(err) => err,
            };
            let text = err.to_string();
            assert!(
                text.contains("屏幕录制权限"),
                "expected Screen Recording guidance, got: {text}"
            );
        }
    }

    #[test]
    #[ignore = "requires macOS Screen Recording TCC permission"]
    fn live_sck_captures_bgra_frame() {
        assert!(
            MacCapturer::has_screen_recording_permission(),
            "Screen Recording permission is not granted for this process"
        );
        let mut capturer = MacCapturer::new(0).expect("create MacCapturer(0)");
        let mut frame = None;
        for _ in 0..20 {
            if let Some(f) = capturer.capture(250).expect("capture") {
                frame = Some(f);
                break;
            }
        }
        let frame = frame.expect("expected at least one ScreenCaptureKit frame");
        assert_eq!(frame.format, PixelFormat::Bgra8);
        assert!(frame.width >= 2 && frame.height >= 2);
        assert_eq!(frame.stride, frame.width * 4);
        assert_eq!(
            frame.data.len(),
            frame.stride as usize * frame.height as usize
        );
        println!(
            "captured live SCK frame: {}x{} ({} bytes)",
            frame.width,
            frame.height,
            frame.data.len()
        );
    }
}
