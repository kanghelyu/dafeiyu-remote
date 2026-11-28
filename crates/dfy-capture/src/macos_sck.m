// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

#import <CoreFoundation/CoreFoundation.h>
#import <CoreGraphics/CoreGraphics.h>
#import <CoreMedia/CoreMedia.h>
#import <CoreVideo/CoreVideo.h>
#import <Foundation/Foundation.h>
#import <ScreenCaptureKit/ScreenCaptureKit.h>

#include <errno.h>
#include <math.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

typedef struct {
    uint32_t display_id;
    int32_t x;
    int32_t y;
    uint32_t w;
    uint32_t h;
    uint32_t primary;
    uint32_t dpi;
} DfyMacMonitor;

typedef struct {
    uint32_t width;
    uint32_t height;
    uint32_t stride;
    uint64_t seq;
    int64_t pts_ns;
    uint8_t *data;
    size_t data_len;
} DfySckFrame;

static void write_err(char *err_buf, size_t err_cap, const char *msg) {
    if (!err_buf || err_cap == 0) {
        return;
    }
    if (!msg) {
        err_buf[0] = '\0';
        return;
    }
    snprintf(err_buf, err_cap, "%s", msg);
}

int32_t dfy_sck_preflight_permission(void) {
    return CGPreflightScreenCaptureAccess() ? 1 : 0;
}

int32_t dfy_sck_enumerate_monitors(
    DfyMacMonitor *out,
    uint32_t cap,
    uint32_t *out_count
) {
    if (!out_count) {
        return -1;
    }
    *out_count = 0;

    CGDirectDisplayID ids[32];
    uint32_t count = 0;
    CGError err = CGGetActiveDisplayList(32, ids, &count);
    if (err != kCGErrorSuccess) {
        return (int32_t)err;
    }

    uint32_t limit = count < cap ? count : cap;
    for (uint32_t i = 0; i < limit; i++) {
        CGDirectDisplayID did = ids[i];
        CGRect bounds = CGDisplayBounds(did);
        size_t pw = (size_t)llround(bounds.size.width > 0.0 ? bounds.size.width : 0.0);
        size_t ph = (size_t)llround(bounds.size.height > 0.0 ? bounds.size.height : 0.0);

        CGDisplayModeRef mode = CGDisplayCopyDisplayMode(did);
        if (mode != NULL) {
            size_t mpw = CGDisplayModeGetPixelWidth(mode);
            size_t mph = CGDisplayModeGetPixelHeight(mode);
            if (mpw > 0 && mph > 0) {
                pw = mpw;
                ph = mph;
            }
            CGDisplayModeRelease(mode);
        }

        uint32_t dpi = 96;
        if (bounds.size.width > 1.0 && pw > 0) {
            double scale = (double)pw / bounds.size.width;
            dpi = (uint32_t)llround(96.0 * scale);
        }

        out[i].display_id = did;
        out[i].x = (int32_t)llround(bounds.origin.x);
        out[i].y = (int32_t)llround(bounds.origin.y);
        out[i].w = (uint32_t)pw;
        out[i].h = (uint32_t)ph;
        out[i].primary = CGDisplayIsMain(did) ? 1u : 0u;
        out[i].dpi = dpi;
    }
    *out_count = limit;
    return 0;
}

API_AVAILABLE(macos(12.3))
@interface DfySckContentResult : NSObject
@property (nonatomic, strong) SCShareableContent *content;
@property (nonatomic, strong) NSError *error;
@end

@implementation DfySckContentResult
@end

API_AVAILABLE(macos(12.3))
@interface DfySckStartResult : NSObject
@property (nonatomic, strong) NSError *error;
@end

@implementation DfySckStartResult
@end

API_AVAILABLE(macos(12.3))
@interface DfySckStream : NSObject <SCStreamOutput, SCStreamDelegate> {
@public
    pthread_mutex_t _lock;
    pthread_cond_t _cond;
    uint8_t *_latestBuf;
    size_t _latestCap;
    uint32_t _latestW;
    uint32_t _latestH;
    uint64_t _latestSeq;
    int64_t _latestPtsNs;
    BOOL _hasFrame;
    BOOL _stopped;
}
@property (nonatomic, strong) SCStream *stream;
@property (nonatomic, strong) dispatch_queue_t sampleQueue;
@property (nonatomic, copy) NSString *streamError;
@end

@implementation DfySckStream

- (instancetype)init {
    self = [super init];
    if (self) {
        pthread_mutex_init(&_lock, NULL);
        pthread_cond_init(&_cond, NULL);
        _latestBuf = NULL;
        _latestCap = 0;
        _latestW = 0;
        _latestH = 0;
        _latestSeq = 0;
        _latestPtsNs = 0;
        _hasFrame = NO;
        _stopped = NO;
    }
    return self;
}

- (void)dealloc {
    free(_latestBuf);
    _latestBuf = NULL;
    pthread_cond_destroy(&_cond);
    pthread_mutex_destroy(&_lock);
}

- (void)stream:(SCStream *)stream
didOutputSampleBuffer:(CMSampleBufferRef)sampleBuffer
        ofType:(SCStreamOutputType)type {
    (void)stream;
    if (type != SCStreamOutputTypeScreen || !sampleBuffer || !CMSampleBufferIsValid(sampleBuffer)) {
        return;
    }

    CFArrayRef attachments = CMSampleBufferGetSampleAttachmentsArray(sampleBuffer, false);
    if (attachments != NULL && CFArrayGetCount(attachments) > 0) {
        NSArray *arr = (__bridge NSArray *)attachments;
        id first = arr.firstObject;
        if ([first isKindOfClass:[NSDictionary class]]) {
            NSNumber *status = ((NSDictionary *)first)[SCStreamFrameInfoStatus];
            if (status != nil && status.integerValue != SCFrameStatusComplete) {
                return;
            }
        }
    }

    CVImageBufferRef pb = CMSampleBufferGetImageBuffer(sampleBuffer);
    if (!pb) {
        return;
    }

    if (CVPixelBufferLockBaseAddress(pb, kCVPixelBufferLock_ReadOnly) != kCVReturnSuccess) {
        return;
    }

    size_t w = CVPixelBufferGetWidth(pb);
    size_t h = CVPixelBufferGetHeight(pb);
    size_t srcStride = CVPixelBufferGetBytesPerRow(pb);
    const uint8_t *base = (const uint8_t *)CVPixelBufferGetBaseAddress(pb);

    if (base != NULL && w > 0 && h > 0 && srcStride >= w * 4) {
        size_t packedRow = w * 4;
        size_t needed = packedRow * h;
        CMTime pts = CMSampleBufferGetPresentationTimeStamp(sampleBuffer);
        int64_t ptsNs = 0;
        if (CMTIME_IS_VALID(pts) && pts.timescale > 0) {
            ptsNs = (int64_t)llround(CMTimeGetSeconds(pts) * 1e9);
        }

        pthread_mutex_lock(&_lock);
        if (!_stopped) {
            if (_latestCap < needed) {
                uint8_t *grown = (uint8_t *)realloc(_latestBuf, needed);
                if (grown != NULL) {
                    _latestBuf = grown;
                    _latestCap = needed;
                }
            }
            if (_latestBuf != NULL && _latestCap >= needed) {
                if (srcStride == packedRow) {
                    memcpy(_latestBuf, base, needed);
                } else {
                    for (size_t row = 0; row < h; row++) {
                        memcpy(_latestBuf + row * packedRow, base + row * srcStride, packedRow);
                    }
                }
                _latestW = (uint32_t)w;
                _latestH = (uint32_t)h;
                _latestPtsNs = ptsNs;
                _latestSeq += 1;
                _hasFrame = YES;
                pthread_cond_signal(&_cond);
            }
        }
        pthread_mutex_unlock(&_lock);
    }

    CVPixelBufferUnlockBaseAddress(pb, kCVPixelBufferLock_ReadOnly);
}

- (void)stream:(SCStream *)stream didStopWithError:(NSError *)error {
    (void)stream;
    pthread_mutex_lock(&_lock);
    _stopped = YES;
    if (error != nil) {
        self.streamError = error.localizedDescription;
    }
    pthread_cond_broadcast(&_cond);
    pthread_mutex_unlock(&_lock);
}

@end

void *dfy_sck_create(
    uint32_t display_id,
    uint32_t width,
    uint32_t height,
    uint32_t max_fps,
    char *err_buf,
    size_t err_cap
) {
    if (!CGPreflightScreenCaptureAccess()) {
        write_err(
            err_buf,
            err_cap,
            "缺少 macOS 屏幕录制权限（Screen Recording）；请在「系统设置 → 隐私与安全性 → 录屏与系统录音」中允许当前应用或终端后重试"
        );
        return NULL;
    }

    if (@available(macOS 12.3, *)) {
        @autoreleasepool {
            DfySckContentResult *contentResult = [[DfySckContentResult alloc] init];
            dispatch_semaphore_t sem = dispatch_semaphore_create(0);
            [SCShareableContent getShareableContentExcludingDesktopWindows:NO
                                                      onScreenWindowsOnly:YES
                                                        completionHandler:^(SCShareableContent *content, NSError *error) {
                contentResult.content = content;
                contentResult.error = error;
                dispatch_semaphore_signal(sem);
            }];
            dispatch_time_t timeout = dispatch_time(DISPATCH_TIME_NOW, (int64_t)5 * NSEC_PER_SEC);
            if (dispatch_semaphore_wait(sem, timeout) != 0) {
                write_err(err_buf, err_cap, "获取 macOS ScreenCaptureKit 可共享显示器列表超时");
                return NULL;
            }
            if (contentResult.error != nil || contentResult.content == nil) {
                NSString *desc = contentResult.error.localizedDescription ?: @"未知错误";
                write_err(err_buf, err_cap, desc.UTF8String);
                return NULL;
            }

            SCDisplay *target = nil;
            for (SCDisplay *disp in contentResult.content.displays) {
                if (disp.displayID == display_id) {
                    target = disp;
                    break;
                }
            }
            if (target == nil) {
                write_err(err_buf, err_cap, "在 ScreenCaptureKit 显示列表中找不到目标显示器");
                return NULL;
            }

            SCContentFilter *filter =
                [[SCContentFilter alloc] initWithDisplay:target excludingWindows:@[]];
            SCStreamConfiguration *cfg = [[SCStreamConfiguration alloc] init];
            uint32_t alignedW = (width & ~1u);
            uint32_t alignedH = (height & ~1u);
            if (alignedW < 2) {
                alignedW = 2;
            }
            if (alignedH < 2) {
                alignedH = 2;
            }
            cfg.width = alignedW;
            cfg.height = alignedH;
            cfg.pixelFormat = kCVPixelFormatType_32BGRA;
            cfg.showsCursor = YES;
            cfg.scalesToFit = YES;
            cfg.queueDepth = 3;
            int32_t fps = (int32_t)(max_fps > 0 ? max_fps : 60);
            cfg.minimumFrameInterval = CMTimeMake(1, fps);

            DfySckStream *holder = [[DfySckStream alloc] init];
            holder.sampleQueue =
                dispatch_queue_create("com.dafeiyu.capture.sck", DISPATCH_QUEUE_SERIAL);
            SCStream *stream =
                [[SCStream alloc] initWithFilter:filter configuration:cfg delegate:holder];
            if (stream == nil) {
                write_err(err_buf, err_cap, "创建 ScreenCaptureKit SCStream 失败");
                return NULL;
            }
            holder.stream = stream;

            NSError *addErr = nil;
            BOOL added = [stream addStreamOutput:holder
                                            type:SCStreamOutputTypeScreen
                              sampleHandlerQueue:holder.sampleQueue
                                           error:&addErr];
            if (!added) {
                NSString *desc = addErr.localizedDescription ?: @"addStreamOutput 失败";
                write_err(err_buf, err_cap, desc.UTF8String);
                return NULL;
            }

            DfySckStartResult *startResult = [[DfySckStartResult alloc] init];
            dispatch_semaphore_t startSem = dispatch_semaphore_create(0);
            [stream startCaptureWithCompletionHandler:^(NSError *error) {
                startResult.error = error;
                dispatch_semaphore_signal(startSem);
            }];
            dispatch_time_t startTimeout =
                dispatch_time(DISPATCH_TIME_NOW, (int64_t)5 * NSEC_PER_SEC);
            if (dispatch_semaphore_wait(startSem, startTimeout) != 0) {
                write_err(err_buf, err_cap, "启动 ScreenCaptureKit 采集流超时");
                return NULL;
            }
            if (startResult.error != nil) {
                NSString *desc = startResult.error.localizedDescription ?: @"启动 SCStream 失败";
                write_err(err_buf, err_cap, desc.UTF8String);
                return NULL;
            }

            return (__bridge_retained void *)holder;
        }
    } else {
        write_err(err_buf, err_cap, "ScreenCaptureKit 需要 macOS 12.3 或更高版本");
        return NULL;
    }
}

int32_t dfy_sck_capture(
    void *handle,
    uint32_t timeout_ms,
    uint64_t *last_seq_inout,
    DfySckFrame *out_frame,
    char *err_buf,
    size_t err_cap
) {
    if (!handle || !last_seq_inout || !out_frame) {
        write_err(err_buf, err_cap, "ScreenCaptureKit 采集句柄为空");
        return -1;
    }
    if (@available(macOS 12.3, *)) {
        DfySckStream *holder = (__bridge DfySckStream *)handle;
        pthread_mutex_lock(&holder->_lock);

        if ((!holder->_hasFrame || holder->_latestSeq == *last_seq_inout) && !holder->_stopped &&
            timeout_ms > 0) {
            struct timespec ts;
            clock_gettime(CLOCK_REALTIME, &ts);
            uint64_t addNs = (uint64_t)timeout_ms * 1000000ULL;
            ts.tv_sec += (time_t)(addNs / 1000000000ULL);
            ts.tv_nsec += (long)(addNs % 1000000000ULL);
            if (ts.tv_nsec >= 1000000000L) {
                ts.tv_sec += 1;
                ts.tv_nsec -= 1000000000L;
            }
            while ((!holder->_hasFrame || holder->_latestSeq == *last_seq_inout) &&
                   !holder->_stopped) {
                int rc = pthread_cond_timedwait(&holder->_cond, &holder->_lock, &ts);
                if (rc == ETIMEDOUT) {
                    break;
                }
            }
        }

        if (holder->_stopped) {
            NSString *msg = holder.streamError ?: @"ScreenCaptureKit 采集流已停止";
            write_err(err_buf, err_cap, msg.UTF8String);
            pthread_mutex_unlock(&holder->_lock);
            return -1;
        }

        if (!holder->_hasFrame || holder->_latestSeq == *last_seq_inout || !holder->_latestBuf) {
            pthread_mutex_unlock(&holder->_lock);
            return 0;
        }

        uint32_t w = holder->_latestW;
        uint32_t h = holder->_latestH;
        uint32_t stride = w * 4u;
        size_t len = (size_t)stride * (size_t)h;
        uint8_t *copy = (uint8_t *)malloc(len);
        if (!copy) {
            pthread_mutex_unlock(&holder->_lock);
            write_err(err_buf, err_cap, "分配屏幕帧内存失败");
            return -1;
        }
        memcpy(copy, holder->_latestBuf, len);
        uint64_t seq = holder->_latestSeq;
        int64_t ptsNs = holder->_latestPtsNs;
        pthread_mutex_unlock(&holder->_lock);

        *last_seq_inout = seq;
        out_frame->width = w;
        out_frame->height = h;
        out_frame->stride = stride;
        out_frame->seq = seq;
        out_frame->pts_ns = ptsNs;
        out_frame->data = copy;
        out_frame->data_len = len;
        return 1;
    } else {
        write_err(err_buf, err_cap, "ScreenCaptureKit 需要 macOS 12.3 或更高版本");
        return -1;
    }
}

void dfy_sck_free_frame(uint8_t *ptr) {
    free(ptr);
}

void dfy_sck_destroy(void *handle) {
    if (!handle) {
        return;
    }
    if (@available(macOS 12.3, *)) {
        @autoreleasepool {
            DfySckStream *holder = (__bridge_transfer DfySckStream *)handle;
            pthread_mutex_lock(&holder->_lock);
            holder->_stopped = YES;
            pthread_cond_broadcast(&holder->_cond);
            pthread_mutex_unlock(&holder->_lock);

            if (holder.stream != nil) {
                dispatch_semaphore_t sem = dispatch_semaphore_create(0);
                [holder.stream stopCaptureWithCompletionHandler:^(NSError *error) {
                    (void)error;
                    dispatch_semaphore_signal(sem);
                }];
                dispatch_time_t timeout =
                    dispatch_time(DISPATCH_TIME_NOW, (int64_t)3 * NSEC_PER_SEC);
                dispatch_semaphore_wait(sem, timeout);
                [holder.stream removeStreamOutput:holder type:SCStreamOutputTypeScreen error:nil];
                holder.stream = nil;
            }
        }
    }
}
