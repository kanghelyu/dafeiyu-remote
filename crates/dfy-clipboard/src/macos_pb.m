// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

#import <AppKit/AppKit.h>
#import <Foundation/Foundation.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

enum {
    DFY_PB_KIND_TEXT = 1,
    DFY_PB_KIND_HTML = 2,
    DFY_PB_KIND_PNG = 3,
    DFY_PB_KIND_FILES = 4,
    DFY_PB_KIND_RTF = 5,
};

enum {
    DFY_PB_MASK_TEXT = 1u << 0,
    DFY_PB_MASK_HTML = 1u << 1,
    DFY_PB_MASK_PNG = 1u << 2,
    DFY_PB_MASK_FILES = 1u << 3,
    DFY_PB_MASK_RTF = 1u << 4,
};

static uint8_t *dfy_pb_copy_bytes(const void *src, size_t len, size_t *out_len) {
    if (out_len != NULL) {
        *out_len = len;
    }
    size_t alloc_len = len > 0 ? len : 1;
    uint8_t *buf = (uint8_t *)malloc(alloc_len);
    if (buf == NULL) {
        return NULL;
    }
    if (len > 0 && src != NULL) {
        memcpy(buf, src, len);
    }
    return buf;
}

static uint8_t *dfy_pb_copy_nsdata(NSData *data, size_t *out_len) {
    if (data == nil) {
        return NULL;
    }
    return dfy_pb_copy_bytes([data bytes], (size_t)[data length], out_len);
}

int64_t dfy_pb_change_count(void) {
    @autoreleasepool {
        NSPasteboard *pb = [NSPasteboard generalPasteboard];
        return (int64_t)[pb changeCount];
    }
}

uint32_t dfy_pb_available_mask(int64_t *out_change_count) {
    @autoreleasepool {
        NSPasteboard *pb = [NSPasteboard generalPasteboard];
        if (out_change_count != NULL) {
            *out_change_count = (int64_t)[pb changeCount];
        }
        uint32_t mask = 0;
        NSArray<NSPasteboardType> *types = [pb types];
        if ([types containsObject:NSPasteboardTypeString]) {
            mask |= DFY_PB_MASK_TEXT;
        }
        if ([types containsObject:NSPasteboardTypeHTML]) {
            mask |= DFY_PB_MASK_HTML;
        }
        if ([types containsObject:NSPasteboardTypePNG] ||
            [types containsObject:NSPasteboardTypeTIFF]) {
            mask |= DFY_PB_MASK_PNG;
        }
        if ([types containsObject:NSPasteboardTypeFileURL]) {
            mask |= DFY_PB_MASK_FILES;
        }
        if ([types containsObject:NSPasteboardTypeRTF]) {
            mask |= DFY_PB_MASK_RTF;
        }
        return mask;
    }
}

uint8_t *dfy_pb_read_kind(uint32_t kind_tag, size_t *out_len) {
    if (out_len != NULL) {
        *out_len = 0;
    }
    @autoreleasepool {
        NSPasteboard *pb = [NSPasteboard generalPasteboard];
        switch (kind_tag) {
            case DFY_PB_KIND_TEXT: {
                NSString *str = [pb stringForType:NSPasteboardTypeString];
                if (str == nil) {
                    return NULL;
                }
                NSData *utf8 = [str dataUsingEncoding:NSUTF8StringEncoding];
                return dfy_pb_copy_nsdata(utf8, out_len);
            }
            case DFY_PB_KIND_HTML: {
                NSData *data = [pb dataForType:NSPasteboardTypeHTML];
                if (data == nil) {
                    NSString *str = [pb stringForType:NSPasteboardTypeHTML];
                    if (str != nil) {
                        data = [str dataUsingEncoding:NSUTF8StringEncoding];
                    }
                }
                return dfy_pb_copy_nsdata(data, out_len);
            }
            case DFY_PB_KIND_PNG: {
                NSData *png = [pb dataForType:NSPasteboardTypePNG];
                if (png != nil) {
                    return dfy_pb_copy_nsdata(png, out_len);
                }
                NSData *tiff = [pb dataForType:NSPasteboardTypeTIFF];
                if (tiff == nil) {
                    return NULL;
                }
                NSBitmapImageRep *rep = [NSBitmapImageRep imageRepWithData:tiff];
                if (rep == nil) {
                    return NULL;
                }
                NSData *converted = [rep representationUsingType:NSBitmapImageFileTypePNG
                                                      properties:@{}];
                return dfy_pb_copy_nsdata(converted, out_len);
            }
            case DFY_PB_KIND_FILES: {
                NSDictionary *opts = @{NSPasteboardURLReadingFileURLsOnlyKey : @YES};
                NSArray *urls = [pb readObjectsForClasses:@[ [NSURL class] ] options:opts];
                if (urls == nil || [urls count] == 0) {
                    return NULL;
                }
                NSMutableData *buf = [NSMutableData data];
                for (id item in urls) {
                    if (![item isKindOfClass:[NSURL class]]) {
                        continue;
                    }
                    NSURL *url = (NSURL *)item;
                    if (![url isFileURL]) {
                        continue;
                    }
                    const char *fs = [url fileSystemRepresentation];
                    if (fs != NULL && fs[0] != '\0') {
                        [buf appendBytes:fs length:strlen(fs)];
                        uint8_t nul = 0;
                        [buf appendBytes:&nul length:1];
                    }
                }
                if ([buf length] == 0) {
                    return NULL;
                }
                return dfy_pb_copy_nsdata(buf, out_len);
            }
            case DFY_PB_KIND_RTF: {
                NSData *data = [pb dataForType:NSPasteboardTypeRTF];
                return dfy_pb_copy_nsdata(data, out_len);
            }
            default:
                return NULL;
        }
    }
}

int32_t dfy_pb_write_bytes(uint32_t kind_tag, const uint8_t *data, size_t len) {
    @autoreleasepool {
        NSPasteboard *pb = [NSPasteboard generalPasteboard];
        [pb clearContents];
        switch (kind_tag) {
            case DFY_PB_KIND_TEXT: {
                NSString *str = @"";
                if (len > 0 && data != NULL) {
                    str = [[NSString alloc] initWithBytes:data
                                                   length:len
                                                 encoding:NSUTF8StringEncoding];
                    if (str == nil) {
                        return -1;
                    }
                }
                return [pb setString:str forType:NSPasteboardTypeString] ? 0 : -1;
            }
            case DFY_PB_KIND_HTML: {
                NSData *nsdata = (len > 0 && data != NULL)
                    ? [NSData dataWithBytes:data length:len]
                    : [NSData data];
                return [pb setData:nsdata forType:NSPasteboardTypeHTML] ? 0 : -1;
            }
            case DFY_PB_KIND_PNG: {
                if (len == 0 || data == NULL) {
                    return -1;
                }
                NSData *nsdata = [NSData dataWithBytes:data length:len];
                return [pb setData:nsdata forType:NSPasteboardTypePNG] ? 0 : -1;
            }
            case DFY_PB_KIND_RTF: {
                NSData *nsdata = (len > 0 && data != NULL)
                    ? [NSData dataWithBytes:data length:len]
                    : [NSData data];
                return [pb setData:nsdata forType:NSPasteboardTypeRTF] ? 0 : -1;
            }
            default:
                return -1;
        }
    }
}

int32_t dfy_pb_write_files(const char *const *paths, size_t count) {
    @autoreleasepool {
        if (count == 0 || paths == NULL) {
            return -1;
        }
        NSMutableArray<NSURL *> *urls = [NSMutableArray arrayWithCapacity:count];
        for (size_t i = 0; i < count; i++) {
            const char *p = paths[i];
            if (p == NULL || p[0] == '\0') {
                continue;
            }
            NSString *ns = [NSString stringWithUTF8String:p];
            if (ns == nil) {
                return -1;
            }
            NSURL *url = [NSURL fileURLWithPath:ns];
            if (url != nil) {
                [urls addObject:url];
            }
        }
        if ([urls count] == 0) {
            return -1;
        }
        NSPasteboard *pb = [NSPasteboard generalPasteboard];
        [pb clearContents];
        return [pb writeObjects:urls] ? 0 : -1;
    }
}

void dfy_pb_free(uint8_t *ptr) {
    free(ptr);
}
