// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva
//
// Spike 1 for docs/design/blob-decode-targets.md: can VideoToolbox be made to
// decode into storage we choose, or must the host copy each frame into the
// decode target's IOSurface?
//
// The design turns on one number — one copy per frame, or none — and everything
// else in it is unchanged either way. There is no per-frame destination-buffer
// API (see RESULTS.md), so the only remaining levers are the session's
// destinationImageBufferAttributes and whatever VideoToolbox's own pool does.
// This measures both, plus the copy we would pay if neither helps.
//
//   cc -O2 -o probe probe.c -framework VideoToolbox -framework CoreMedia \
//      -framework CoreVideo -framework IOSurface -framework CoreFoundation
//
//   ./probe layout ../vt-vp9-decode/hidden-frames.ivf
//   ./probe pool   ../vt-vp9-decode/hidden-frames.ivf
//   ./probe copy
//   ./probe alloc
//
// The IVF reader and superframe splitter are lifted from vt-vp9-decode.c, which
// established that this fixture round-trips every frame including the hidden
// ones. Decode flags stay 0 to match how vrend_video.c actually drives VT.

#include <CoreFoundation/CoreFoundation.h>
#include <CoreMedia/CoreMedia.h>
#include <CoreVideo/CoreVideo.h>
#include <IOSurface/IOSurface.h>
#include <VideoToolbox/VideoToolbox.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#define MAX_HELD 256

static uint64_t now_ns(void)
{
    return clock_gettime_nsec_np(CLOCK_MONOTONIC);
}

/* ---------------------------------------------------------------- IVF input */

struct ivf {
    uint8_t *data;
    size_t size;
    unsigned width, height;
};

static bool ivf_load(const char *path, struct ivf *out)
{
    FILE *f = fopen(path, "rb");
    if (!f) {
        perror(path);
        return false;
    }
    fseek(f, 0, SEEK_END);
    long len = ftell(f);
    fseek(f, 0, SEEK_SET);
    if (len < 32) {
        fclose(f);
        return false;
    }
    out->data = malloc((size_t)len);
    if (fread(out->data, 1, (size_t)len, f) != (size_t)len || memcmp(out->data, "DKIF", 4) != 0) {
        fprintf(stderr, "%s: not an IVF file\n", path);
        fclose(f);
        return false;
    }
    fclose(f);
    out->size = (size_t)len;
    out->width = out->data[12] | (out->data[13] << 8);
    out->height = out->data[14] | (out->data[15] << 8);
    return true;
}

/* VP9 superframes pack several frames plus a trailing index; the guest splits
 * them before VA-API ever sees one, so we do the same. */
static int split_superframe(const uint8_t *data, size_t size, const uint8_t **frames,
                            size_t *sizes, int max)
{
    if (size < 1)
        return 0;

    uint8_t marker = data[size - 1];
    if ((marker & 0xe0) == 0xc0) {
        int frames_in = (marker & 0x7) + 1;
        int mag = ((marker >> 3) & 0x3) + 1;
        size_t index_sz = 2 + (size_t)mag * frames_in;

        if (size >= index_sz && data[size - index_sz] == marker) {
            const uint8_t *idx = data + size - index_sz + 1;
            size_t off = 0;
            int n = 0;
            for (int i = 0; i < frames_in && n < max; i++) {
                size_t sz = 0;
                for (int j = 0; j < mag; j++)
                    sz |= (size_t)(*idx++) << (j * 8);
                if (off + sz > size)
                    break;
                frames[n] = data + off;
                sizes[n] = sz;
                n++;
                off += sz;
            }
            return n;
        }
    }
    if (max < 1)
        return 0;
    frames[0] = data;
    sizes[0] = size;
    return 1;
}

/* show_existing_frame re-presents an already-decoded surface and never reaches a
 * VA driver, so it is skipped here too. */
static bool is_show_existing(const uint8_t *d, size_t size)
{
    if (size < 1)
        return false;
    unsigned marker = (d[0] >> 6) & 3;
    if (marker != 2)
        return false;
    unsigned profile = ((d[0] >> 4) & 1) << 1 | ((d[0] >> 5) & 1);
    unsigned bit = profile == 3 ? 2 : 3; /* bit index of show_existing_frame */
    return (d[0] >> (3 - bit)) & 1;
}

static CFDataRef make_vpcc(void)
{
    uint8_t box[12] = {0};
    box[0] = 1;  /* version */
    box[4] = 0;  /* profile 0 */
    box[5] = 41; /* level 4.1, advisory */
    box[6] = (8 << 4) | (1 << 1); /* 8-bit, 4:2:0 colocated, studio range */
    box[7] = box[8] = box[9] = 2; /* unspecified colour */
    return CFDataCreate(kCFAllocatorDefault, box, sizeof(box));
}

static CMVideoFormatDescriptionRef make_format(unsigned w, unsigned h)
{
    CFDataRef vpcc = make_vpcc();
    CFMutableDictionaryRef atoms = CFDictionaryCreateMutable(
        kCFAllocatorDefault, 1, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    CFDictionarySetValue(atoms, CFSTR("vpcC"), vpcc);
    CFMutableDictionaryRef ext = CFDictionaryCreateMutable(
        kCFAllocatorDefault, 1, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    CFDictionarySetValue(ext, kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms, atoms);

    CMVideoFormatDescriptionRef fmt = NULL;
    OSStatus st = CMVideoFormatDescriptionCreate(kCFAllocatorDefault, kCMVideoCodecType_VP9, w, h,
                                                 ext, &fmt);
    CFRelease(ext);
    CFRelease(atoms);
    CFRelease(vpcc);
    if (st != noErr) {
        fprintf(stderr, "CMVideoFormatDescriptionCreate failed: %d\n", (int)st);
        return NULL;
    }
    return fmt;
}

/* ------------------------------------------------------------- decode driver */

struct run {
    int outputs;
    int errors;
    int nonzero;

    /* Layout of the first output, read off the IOSurface rather than the
     * CVPixelBuffer, because the design's "one object, two layers" claim is
     * about the surface's own geometry. */
    bool saw_surface;
    bool iosurface_backed;
    size_t alloc_size;
    size_t plane_count;
    size_t plane_bpr[4];
    size_t plane_off[4];
    size_t plane_w[4], plane_h[4];
    OSType pixel_format;

    /* Pool behaviour. */
    IOSurfaceID ids[1024];
    int id_count;
    bool hold;
    CVPixelBufferRef held[MAX_HELD];
    int held_count;
};

static void note_id(struct run *r, IOSurfaceID id)
{
    if (r->id_count < (int)(sizeof(r->ids) / sizeof(r->ids[0])))
        r->ids[r->id_count++] = id;
}

static void on_output(void *ref_con, void *frame_ref, OSStatus status, VTDecodeInfoFlags flags,
                      CVImageBufferRef img, CMTime pts, CMTime dur)
{
    struct run *r = ref_con;
    (void)frame_ref;
    (void)flags;
    (void)pts;
    (void)dur;

    if (status != noErr) {
        r->errors++;
        return;
    }
    if (!img)
        return;
    r->outputs++;

    IOSurfaceRef s = CVPixelBufferGetIOSurface(img);
    if (s)
        note_id(r, IOSurfaceGetID(s));

    if (!r->saw_surface) {
        r->saw_surface = true;
        r->iosurface_backed = s != NULL;
        r->pixel_format = CVPixelBufferGetPixelFormatType(img);
        if (s) {
            r->alloc_size = IOSurfaceGetAllocSize(s);
            r->plane_count = IOSurfaceGetPlaneCount(s);
            uint8_t *base = IOSurfaceGetBaseAddress(s);
            for (size_t i = 0; i < r->plane_count && i < 4; i++) {
                r->plane_bpr[i] = IOSurfaceGetBytesPerRowOfPlane(s, i);
                r->plane_off[i] =
                    (size_t)((uint8_t *)IOSurfaceGetBaseAddressOfPlane(s, i) - base);
                r->plane_w[i] = IOSurfaceGetWidthOfPlane(s, i);
                r->plane_h[i] = IOSurfaceGetHeightOfPlane(s, i);
            }
        }
    }

    /* "A buffer came back" is not "it decoded"; read the luma. */
    CVPixelBufferLockBaseAddress(img, kCVPixelBufferLock_ReadOnly);
    const uint8_t *y = CVPixelBufferGetBaseAddressOfPlane(img, 0);
    size_t stride = CVPixelBufferGetBytesPerRowOfPlane(img, 0);
    size_t h = CVPixelBufferGetHeightOfPlane(img, 0);
    size_t w = CVPixelBufferGetWidthOfPlane(img, 0);
    bool varies = false;
    if (y && h > 1) {
        uint8_t first = y[0];
        for (size_t row = 0; row < h && !varies; row += 4)
            for (size_t col = 0; col < w; col += 4)
                if (y[row * stride + col] != first) {
                    varies = true;
                    break;
                }
    }
    CVPixelBufferUnlockBaseAddress(img, kCVPixelBufferLock_ReadOnly);
    if (varies)
        r->nonzero++;

    /* Holding outputs is the only thing that could resurrect "map the whole pool
     * into the guest once": it asks whether the pool is bounded and recycling or
     * simply grows on demand. */
    if (r->hold && r->held_count < MAX_HELD)
        r->held[r->held_count++] = (CVPixelBufferRef)CFRetain(img);
}

/* Runs the whole fixture through one session and fills `r`. Returns the wall
 * time in nanoseconds, or 0 if the session could not be created. */
static uint64_t decode_all(const struct ivf *ivf, CFDictionaryRef attrs, struct run *r,
                           bool *hw_accel, OSStatus *create_status)
{
    CMVideoFormatDescriptionRef fmt = make_format(ivf->width, ivf->height);
    if (!fmt)
        return 0;

    VTDecompressionOutputCallbackRecord cb = {on_output, r};
    VTDecompressionSessionRef session = NULL;
    OSStatus st =
        VTDecompressionSessionCreate(kCFAllocatorDefault, fmt, NULL, attrs, &cb, &session);
    *create_status = st;
    if (st != noErr) {
        CFRelease(fmt);
        return 0;
    }

    /* A constrained layout that "works" may have quietly dropped off the
     * hardware decoder, which would read as a pass. Ask the session, not the
     * codec — the distinction cost a day already (virglrenderer d7dd10aa). */
    *hw_accel = false;
    CFBooleanRef hw = NULL;
    if (VTSessionCopyProperty(session, kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder,
                              kCFAllocatorDefault, &hw) == noErr && hw) {
        *hw_accel = CFBooleanGetValue(hw);
        CFRelease(hw);
    }

    uint64_t t0 = now_ns();

    size_t pos = 32;
    while (pos + 12 <= ivf->size) {
        const uint8_t *fh = ivf->data + pos;
        size_t sz = (size_t)fh[0] | ((size_t)fh[1] << 8) | ((size_t)fh[2] << 16) |
                    ((size_t)fh[3] << 24);
        pos += 12;
        if (pos + sz > ivf->size)
            break;
        const uint8_t *packet = ivf->data + pos;
        pos += sz;

        const uint8_t *frames[8];
        size_t sizes[8];
        int n = split_superframe(packet, sz, frames, sizes, 8);
        for (int i = 0; i < n; i++) {
            if (is_show_existing(frames[i], sizes[i]))
                continue;

            CMBlockBufferRef bb = NULL;
            if (CMBlockBufferCreateWithMemoryBlock(kCFAllocatorDefault, (void *)frames[i],
                                                   sizes[i], kCFAllocatorNull, NULL, 0, sizes[i],
                                                   0, &bb) != noErr)
                continue;
            CMSampleBufferRef sb = NULL;
            const size_t sample_size = sizes[i];
            if (CMSampleBufferCreate(kCFAllocatorDefault, bb, TRUE, NULL, NULL, fmt, 1, 0, NULL, 1,
                                     &sample_size, &sb) != noErr) {
                CFRelease(bb);
                continue;
            }
            VTDecodeInfoFlags info = 0;
            VTDecompressionSessionDecodeFrame(session, sb, 0, NULL, &info);
            CFRelease(sb);
            CFRelease(bb);
        }
    }
    VTDecompressionSessionWaitForAsynchronousFrames(session);
    uint64_t elapsed = now_ns() - t0;

    VTDecompressionSessionInvalidate(session);
    CFRelease(session);
    CFRelease(fmt);
    return elapsed;
}

/* ------------------------------------------------------- attribute builders */

static CFMutableDictionaryRef base_attrs(bool iosurface)
{
    CFMutableDictionaryRef d = CFDictionaryCreateMutable(
        kCFAllocatorDefault, 4, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    int32_t pixfmt = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;
    CFNumberRef n = CFNumberCreate(kCFAllocatorDefault, kCFNumberSInt32Type, &pixfmt);
    CFDictionarySetValue(d, kCVPixelBufferPixelFormatTypeKey, n);
    CFRelease(n);
    if (iosurface) {
        CFDictionaryRef empty =
            CFDictionaryCreate(kCFAllocatorDefault, NULL, NULL, 0, &kCFTypeDictionaryKeyCallBacks,
                               &kCFTypeDictionaryValueCallBacks);
        CFDictionarySetValue(d, kCVPixelBufferIOSurfacePropertiesKey, empty);
        CFRelease(empty);
    }
    return d;
}

static void set_num(CFMutableDictionaryRef d, CFStringRef key, int64_t v)
{
    CFNumberRef n = CFNumberCreate(kCFAllocatorDefault, kCFNumberSInt64Type, &v);
    CFDictionarySetValue(d, key, n);
    CFRelease(n);
}

/* ------------------------------------------------------------ probe: layout */

static int distinct_ids(const struct run *r)
{
    int n = 0;
    for (int i = 0; i < r->id_count; i++) {
        bool seen = false;
        for (int j = 0; j < i; j++)
            if (r->ids[j] == r->ids[i]) {
                seen = true;
                break;
            }
        if (!seen)
            n++;
    }
    return n;
}

static void report(const char *label, const struct run *r, uint64_t ns, bool hw, OSStatus st,
                   size_t asked_align)
{
    printf("  %-34s ", label);
    if (st != noErr) {
        printf("session create FAILED: %d\n", (int)st);
        return;
    }
    if (!r->saw_surface) {
        printf("no outputs (errors %d)\n", r->errors);
        return;
    }
    printf("hw=%-3s  %5.1f ms  outputs %d/%d real\n", hw ? "yes" : "NO", ns / 1e6, r->outputs,
           r->nonzero);
    printf("  %-34s %.4s  IOSurface=%s  allocSize %zu  planes %zu\n", "",
           (char *)&(OSType){CFSwapInt32HostToBig(r->pixel_format)},
           r->iosurface_backed ? "yes" : "NO", r->alloc_size, r->plane_count);
    for (size_t i = 0; i < r->plane_count && i < 4; i++)
        printf("  %-34s   plane %zu: %zux%zu  bytesPerRow %zu  offset %zu\n", "", i, r->plane_w[i],
               r->plane_h[i], r->plane_bpr[i], r->plane_off[i]);
    if (asked_align)
        printf("  %-34s   requested row alignment %zu: %s\n", "", asked_align,
               r->plane_bpr[0] % asked_align == 0 ? "HONOURED" : "IGNORED");
}

static int probe_layout(const char *path)
{
    struct ivf ivf;
    if (!ivf_load(path, &ivf))
        return 1;

    VTRegisterSupplementalVideoDecoderIfAvailable(kCMVideoCodecType_VP9);
    printf("fixture %ux%u   VTIsHardwareDecodeSupported(VP9)=%s\n\n", ivf.width, ivf.height,
           VTIsHardwareDecodeSupported(kCMVideoCodecType_VP9) ? "yes" : "no");

    printf("Does the session honour a layout we dictate?\n");
    printf("(a silently-software session or a hidden conversion both read as a pass,\n");
    printf(" so hw= and the wall time are part of the answer)\n\n");

    struct {
        const char *label;
        bool iosurface;
        size_t row_align;
        size_t plane_align;
        int64_t pool_min;
    } cases[] = {
        {"baseline, IOSurface-backed", true, 0, 0, 0},
        {"no IOSurface properties", false, 0, 0, 0},
        {"row alignment 16", true, 16, 0, 0},
        {"row alignment 64", true, 64, 0, 0},
        {"row alignment 256", true, 256, 0, 0},
        {"row alignment 4096", true, 4096, 0, 0},
        {"row alignment 16384 (host page)", true, 16384, 0, 0},
        {"plane alignment 16384", true, 0, 16384, 0},
        {"pool minimum 32 buffers", true, 0, 0, 32},
    };

    for (size_t c = 0; c < sizeof(cases) / sizeof(cases[0]); c++) {
        CFMutableDictionaryRef d = base_attrs(cases[c].iosurface);
        if (cases[c].row_align)
            set_num(d, kCVPixelBufferBytesPerRowAlignmentKey, (int64_t)cases[c].row_align);
        if (cases[c].plane_align)
            set_num(d, kCVPixelBufferPlaneAlignmentKey, (int64_t)cases[c].plane_align);
        if (cases[c].pool_min)
            set_num(d, kCVPixelBufferPoolMinimumBufferCountKey, cases[c].pool_min);

        struct run r = {0};
        bool hw = false;
        OSStatus st = noErr;
        uint64_t ns = decode_all(&ivf, d, &r, &hw, &st);
        report(cases[c].label, &r, ns, hw, st, cases[c].row_align);
        if (cases[c].pool_min && r.saw_surface)
            printf("  %-34s   distinct surfaces over %d outputs: %d\n", "", r.outputs,
                   distinct_ids(&r));
        printf("\n");
        CFRelease(d);
    }

    free(ivf.data);
    return 0;
}

/* -------------------------------------------------------------- probe: pool */

static int probe_pool(const char *path)
{
    struct ivf ivf;
    if (!ivf_load(path, &ivf))
        return 1;

    VTRegisterSupplementalVideoDecoderIfAvailable(kCMVideoCodecType_VP9);

    printf("Is VideoToolbox's output pool bounded and recycling?\n");
    printf("If it is, mapping every pool surface into the guest once would be an\n");
    printf("alternative to copying. Two hold policies, because the answer differs:\n");
    printf("  release-in-callback approximates how vrend uses the buffer today;\n");
    printf("  hold-all asks whether the pool is a fixed set at all.\n\n");

    for (int hold = 0; hold < 2; hold++) {
        CFMutableDictionaryRef d = base_attrs(true);
        struct run r = {0};
        r.hold = hold != 0;
        bool hw = false;
        OSStatus st = noErr;
        uint64_t ns = decode_all(&ivf, d, &r, &hw, &st);

        printf("  %-22s outputs %d, distinct IOSurface ids %d, %.1f ms, hw=%s\n",
               hold ? "hold every output:" : "release in callback:", r.outputs, distinct_ids(&r),
               ns / 1e6, hw ? "yes" : "NO");
        if (r.id_count > 0) {
            printf("  %-22s first ids:", "");
            for (int i = 0; i < r.id_count && i < 16; i++)
                printf(" %u", r.ids[i]);
            printf("%s\n", r.id_count > 16 ? " ..." : "");
        }
        for (int i = 0; i < r.held_count; i++)
            CFRelease(r.held[i]);
        CFRelease(d);
        printf("\n");
    }

    free(ivf.data);
    return 0;
}

/* -------------------------------------------------------------- probe: copy */

/* Allocate an NV12 IOSurface with a caller-chosen luma pitch, the way the host
 * would have to for a guest-dictated layout. Returns NULL if IOSurface refuses. */
static IOSurfaceRef make_nv12(size_t w, size_t h, size_t luma_pitch)
{
    if (luma_pitch == 0)
        luma_pitch = w;
    /* Chroma is two bytes per 2x2 block, so it needs ceil(w/2)*2 bytes per row --
     * which for an odd width EXCEEDS a luma pitch of exactly w. Size it in its own
     * right rather than inheriting luma's pitch, or odd widths fail for a reason
     * that belongs to the arithmetic here and not to IOSurface. */
    size_t chroma_min = ((w + 1) / 2) * 2;
    size_t chroma_pitch = luma_pitch < chroma_min ? chroma_min : luma_pitch;
    size_t luma_size = luma_pitch * h;
    size_t chroma_size = chroma_pitch * ((h + 1) / 2);

    CFMutableArrayRef planes =
        CFArrayCreateMutable(kCFAllocatorDefault, 2, &kCFTypeArrayCallBacks);
    struct {
        size_t w, h, bpr, off, size, bpe;
    } p[2] = {
        {w, h, luma_pitch, 0, luma_size, 1},
        {(w + 1) / 2, (h + 1) / 2, chroma_pitch, luma_size, chroma_size, 2},
    };
    for (int i = 0; i < 2; i++) {
        CFMutableDictionaryRef pd =
            CFDictionaryCreateMutable(kCFAllocatorDefault, 6, &kCFTypeDictionaryKeyCallBacks,
                                      &kCFTypeDictionaryValueCallBacks);
        set_num(pd, kIOSurfacePlaneWidth, (int64_t)p[i].w);
        set_num(pd, kIOSurfacePlaneHeight, (int64_t)p[i].h);
        set_num(pd, kIOSurfacePlaneBytesPerRow, (int64_t)p[i].bpr);
        set_num(pd, kIOSurfacePlaneOffset, (int64_t)p[i].off);
        set_num(pd, kIOSurfacePlaneSize, (int64_t)p[i].size);
        set_num(pd, kIOSurfacePlaneBytesPerElement, (int64_t)p[i].bpe);
        CFArrayAppendValue(planes, pd);
        CFRelease(pd);
    }

    CFMutableDictionaryRef d = CFDictionaryCreateMutable(
        kCFAllocatorDefault, 5, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    set_num(d, kIOSurfaceWidth, (int64_t)w);
    set_num(d, kIOSurfaceHeight, (int64_t)h);
    set_num(d, kIOSurfacePixelFormat, (int64_t)kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange);
    set_num(d, kIOSurfaceAllocSize, (int64_t)(luma_size + chroma_size));
    CFDictionarySetValue(d, kIOSurfacePlaneInfo, planes);
    CFRelease(planes);

    IOSurfaceRef s = IOSurfaceCreate(d);
    CFRelease(d);
    return s;
}

static int probe_copy(void)
{
    printf("What one copy into the decode target's own storage costs.\n");
    printf("Both shapes matter: matching pitches let the whole plane go in one\n");
    printf("memcpy, mismatched pitches force it row by row, and which one the real\n");
    printf("path gets is decided by whether the layout probe found an honoured\n");
    printf("alignment.\n\n");

    struct {
        const char *name;
        size_t w, h;
    } sizes[] = {{"1080p", 1920, 1080}, {"4K", 3840, 2160}};

    for (size_t i = 0; i < sizeof(sizes) / sizeof(sizes[0]); i++) {
        size_t w = sizes[i].w, h = sizes[i].h;

        /* Tight pitch on both ends = the whole-plane case. A +64 pitch on the
         * destination is the mismatched case. */
        for (int mismatch = 0; mismatch < 2; mismatch++) {
            IOSurfaceRef src = make_nv12(w, h, w);
            IOSurfaceRef dst = make_nv12(w, h, mismatch ? w + 64 : w);
            if (!src || !dst) {
                printf("  %-6s %-22s IOSurfaceCreate refused the layout\n", sizes[i].name,
                       mismatch ? "mismatched pitch:" : "matching pitch:");
                if (src)
                    CFRelease(src);
                if (dst)
                    CFRelease(dst);
                continue;
            }

            IOSurfaceLock(src, 0, NULL);
            IOSurfaceLock(dst, 0, NULL);
            memset(IOSurfaceGetBaseAddress(src), 0x7f, IOSurfaceGetAllocSize(src));

            const int reps = 50;
            uint64_t best = UINT64_MAX;
            uint64_t total = 0;
            for (int rep = 0; rep < reps; rep++) {
                uint64_t t0 = now_ns();
                for (size_t pl = 0; pl < 2; pl++) {
                    uint8_t *sp = IOSurfaceGetBaseAddressOfPlane(src, pl);
                    uint8_t *dp = IOSurfaceGetBaseAddressOfPlane(dst, pl);
                    size_t sbpr = IOSurfaceGetBytesPerRowOfPlane(src, pl);
                    size_t dbpr = IOSurfaceGetBytesPerRowOfPlane(dst, pl);
                    size_t rows = IOSurfaceGetHeightOfPlane(dst, pl);
                    if (sbpr == dbpr) {
                        memcpy(dp, sp, sbpr * rows);
                    } else {
                        size_t run = sbpr < dbpr ? sbpr : dbpr;
                        for (size_t y = 0; y < rows; y++)
                            memcpy(dp + y * dbpr, sp + y * sbpr, run);
                    }
                }
                uint64_t d = now_ns() - t0;
                total += d;
                if (d < best)
                    best = d;
            }
            IOSurfaceUnlock(dst, 0, NULL);
            IOSurfaceUnlock(src, 0, NULL);

            size_t bytes = IOSurfaceGetAllocSize(src);
            printf("  %-6s %-22s best %6.3f ms  mean %6.3f ms  (%.1f GB/s best, %zu bytes)\n",
                   sizes[i].name, mismatch ? "row by row:" : "whole plane:", best / 1e6,
                   (total / reps) / 1e6, bytes / (double)best, bytes);

            CFRelease(src);
            CFRelease(dst);
        }
    }
    printf("\n  For scale, 60 fps leaves 16.7 ms per frame. These reuse one warm pair of\n");
    printf("  surfaces, so best is a floor; the mean is the honest figure to design to.\n");
    return 0;
}

/* ------------------------------------------------------------- probe: alloc */

static int probe_alloc(void)
{
    printf("Can IOSurface be allocated at a pitch the guest chose?\n");
    printf("The design has the guest compute layout and the host allocate to match\n");
    printf("or refuse, so what IOSurface silently rounds is load-bearing.\n\n");

    printf("  IOSurfaceGetPropertyAlignment(kIOSurfaceBytesPerRow)      = %zu\n",
           IOSurfaceGetPropertyAlignment(kIOSurfaceBytesPerRow));
    printf("  IOSurfaceGetPropertyAlignment(kIOSurfacePlaneBytesPerRow) = %zu\n",
           IOSurfaceGetPropertyAlignment(kIOSurfacePlaneBytesPerRow));
    printf("  IOSurfaceGetPropertyAlignment(kIOSurfacePlaneOffset)      = %zu\n",
           IOSurfaceGetPropertyAlignment(kIOSurfacePlaneOffset));
    printf("  IOSurfaceGetPropertyAlignment(kIOSurfaceAllocSize)        = %zu\n\n",
           IOSurfaceGetPropertyAlignment(kIOSurfaceAllocSize));

    struct {
        const char *name;
        size_t w, h, pitch;
    } cases[] = {
        {"854x480 tight (odd width)", 854, 480, 854},
        {"854x480 pitch 864", 854, 480, 864},
        {"1920x1080 tight", 1920, 1080, 1920},
        {"1921x1080 tight (odd width)", 1921, 1080, 1921},
        {"1920x1081 tight (odd height)", 1920, 1081, 1920},
        {"1921x1081 tight (odd both)", 1921, 1081, 1921},
        {"3840x2160 tight", 3840, 2160, 3840},
        {"3840x2160 pitch 16384-aligned", 3840, 2160, 16384},
    };

    for (size_t i = 0; i < sizeof(cases) / sizeof(cases[0]); i++) {
        IOSurfaceRef s = make_nv12(cases[i].w, cases[i].h, cases[i].pitch);
        if (!s) {
            printf("  %-32s REFUSED\n", cases[i].name);
            continue;
        }
        size_t bpr0 = IOSurfaceGetBytesPerRowOfPlane(s, 0);
        size_t bpr1 = IOSurfaceGetBytesPerRowOfPlane(s, 1);
        size_t off1 = (size_t)((uint8_t *)IOSurfaceGetBaseAddressOfPlane(s, 1) -
                               (uint8_t *)IOSurfaceGetBaseAddress(s));
        printf("  %-32s asked %5zu -> luma bpr %5zu %s, chroma bpr %5zu, "
               "chroma offset %8zu, allocSize %9zu\n",
               cases[i].name, cases[i].pitch, bpr0,
               bpr0 == cases[i].pitch ? "(exact)" : "(ROUNDED)", bpr1, off1,
               IOSurfaceGetAllocSize(s));
        CFRelease(s);
    }
    return 0;
}

int main(int argc, char **argv)
{
    if (argc < 2) {
        fprintf(stderr, "usage: %s layout|pool <file.ivf>\n       %s copy|alloc\n", argv[0],
                argv[0]);
        return 2;
    }
    if (strcmp(argv[1], "copy") == 0)
        return probe_copy();
    if (strcmp(argv[1], "alloc") == 0)
        return probe_alloc();
    if (argc < 3) {
        fprintf(stderr, "%s %s needs an IVF file\n", argv[0], argv[1]);
        return 2;
    }
    if (strcmp(argv[1], "layout") == 0)
        return probe_layout(argv[2]);
    if (strcmp(argv[1], "pool") == 0)
        return probe_pool(argv[2]);

    fprintf(stderr, "unknown probe %s\n", argv[1]);
    return 2;
}
