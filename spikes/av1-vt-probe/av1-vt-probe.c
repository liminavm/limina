// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva
//
// What does VideoToolbox's AV1 decoder actually require and return?
//
// The VP9 backend was cheap because VideoToolbox took whole frames and owned its
// own DPB, so the guest's already-parsed picture descriptor could be ignored. AV1
// cannot work that way: the virgl protocol hands over TILES plus a fully parsed
// frame header, so a backend must re-serialize an OBU_FRAME_HEADER. Before anyone
// writes that serializer, three things have to be true of VideoToolbox itself, and
// only an M3-or-later Mac can answer them:
//
//   1. AV1 decodes in hardware here at all.
//   2. It accepts one TEMPORAL UNIT per sample, framed low-overhead (not annexb).
//   3. Its output cardinality. AV1's no-show frames (show_frame=0, displayed later
//      by show_existing_frame) are the analogue of VP9's hidden alt-refs, and the
//      VP9 answer -- one picture out per frame in -- is what let the backend map
//      the protocol 1:1. If VT instead emits only DISPLAYED frames, that mapping
//      is gone and the backend owes a reorder buffer on top of the serializer.
//
// Question 3 is the one that changes the design, so this counts three separate
// things and prints them side by side rather than asserting any of them.
//
//   cc -O2 -o av1-vt-probe av1-vt-probe.c \
//      -framework VideoToolbox -framework CoreMedia -framework CoreVideo -framework CoreFoundation
//   ./av1-vt-probe sample.obu av1C.bin
//
// av1C.bin is the AV1CodecConfigurationRecord lifted out of a real MP4 by
// make-sample.sh. Synthesizing one from virgl's parsed fields is the backend's
// job later; using a muxer's here keeps this probe about VideoToolbox.

#include <CoreFoundation/CoreFoundation.h>
#include <CoreMedia/CoreMedia.h>
#include <CoreVideo/CoreVideo.h>
#include <VideoToolbox/VideoToolbox.h>
#include <pthread.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define OBU_SEQUENCE_HEADER    1
#define OBU_TEMPORAL_DELIMITER 2
#define OBU_FRAME_HEADER       3
#define OBU_FRAME              6

/* leb128 as AV1 spec 4.10.5. Returns bytes consumed, 0 on malformed input. */
static size_t read_leb128(const uint8_t *p, size_t avail, uint64_t *out)
{
    uint64_t value = 0;
    for (size_t i = 0; i < 8 && i < avail; i++) {
        value |= (uint64_t)(p[i] & 0x7f) << (i * 7);
        if (!(p[i] & 0x80)) {
            *out = value;
            return i + 1;
        }
    }
    return 0;
}

struct tu {
    const uint8_t *data;
    size_t size;
    int frame_obus;   /* OBU_FRAME + OBU_FRAME_HEADER carried in this TU */
};

/* Split a low-overhead OBU stream into temporal units. A TU begins at each
 * OBU_TEMPORAL_DELIMITER, which is exactly the framing VideoToolbox wants per
 * sample -- so this is the same split the real backend would have to do. */
static int split_temporal_units(const uint8_t *data, size_t size,
                                struct tu *tus, int max_tus, int *seq_headers)
{
    int count = 0;
    size_t pos = 0;

    *seq_headers = 0;

    while (pos < size) {
        uint8_t header = data[pos];
        int type = (header >> 3) & 0xf;
        bool has_extension = (header >> 2) & 1;
        bool has_size = (header >> 1) & 1;
        size_t obu_start = pos;
        size_t cursor = pos + 1;

        if (has_extension)
            cursor++;
        if (!has_size) {
            fprintf(stderr, "OBU without a size field at %zu; not a low-overhead stream\n", pos);
            return -1;
        }

        uint64_t payload = 0;
        size_t leb = read_leb128(data + cursor, size - cursor, &payload);
        if (!leb)
            return -1;
        cursor += leb + payload;
        if (cursor > size) {
            fprintf(stderr, "OBU at %zu runs past the end of the stream\n", obu_start);
            return -1;
        }

        if (type == OBU_TEMPORAL_DELIMITER) {
            if (count == max_tus)
                return count;
            tus[count].data = data + obu_start;
            tus[count].size = 0;
            tus[count].frame_obus = 0;
            count++;
        }
        if (count) {
            tus[count - 1].size = cursor - (size_t)(tus[count - 1].data - data);
            if (type == OBU_FRAME || type == OBU_FRAME_HEADER)
                tus[count - 1].frame_obus++;
        }
        if (type == OBU_SEQUENCE_HEADER)
            (*seq_headers)++;

        pos = cursor;
    }
    return count;
}

struct decode_stats {
    int outputs;
    int nonzero;
    int errors;
    int null_images;
    int after_return;
    bool in_decode_call;
    OSType pixel_format;
    int planes;
    pthread_t cb_thread;
    size_t last_w, last_h;
    FILE *dump;   /* optional: every picture's luma plane, for an external diff */
};

static void output_callback(void *decompression_output_ref_con, void *source_frame_ref_con,
                            OSStatus status, VTDecodeInfoFlags info_flags,
                            CVImageBufferRef image_buffer, CMTime pts, CMTime duration)
{
    struct decode_stats *st = decompression_output_ref_con;
    (void)source_frame_ref_con;
    (void)info_flags;
    (void)pts;
    (void)duration;

    if (!st->in_decode_call)
        st->after_return++;

    if (status != noErr) {
        st->errors++;
        fprintf(stderr, "  output callback: status %d\n", (int)status);
        return;
    }
    if (!image_buffer) {
        st->null_images++;
        return;
    }

    st->outputs++;
    st->cb_thread = pthread_self();
    st->pixel_format = CVPixelBufferGetPixelFormatType(image_buffer);
    st->planes = (int)CVPixelBufferGetPlaneCount(image_buffer);
    st->last_w = CVPixelBufferGetWidth(image_buffer);
    st->last_h = CVPixelBufferGetHeight(image_buffer);

    /* "It returned a buffer" is not "it decoded anything" -- read the luma. */
    CVPixelBufferLockBaseAddress(image_buffer, kCVPixelBufferLock_ReadOnly);
    const uint8_t *y = CVPixelBufferGetBaseAddressOfPlane(image_buffer, 0);
    size_t stride = CVPixelBufferGetBytesPerRowOfPlane(image_buffer, 0);
    size_t h = CVPixelBufferGetHeightOfPlane(image_buffer, 0);
    size_t w = CVPixelBufferGetWidthOfPlane(image_buffer, 0);
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
    /* Film grain is normative in AV1 but OPTIONAL to apply, and the protocol wants
     * both a grain-free reference and a grain-applied picture. Which one VideoToolbox
     * hands back is not something to reason about -- dump the luma and diff it
     * against dav1d with grain applied and with grain off. */
    if (st->dump && y) {
        for (size_t row = 0; row < h; row++)
            fwrite(y + row * stride, 1, w, st->dump);
    }
    CVPixelBufferUnlockBaseAddress(image_buffer, kCVPixelBufferLock_ReadOnly);
    if (varies)
        st->nonzero++;
}

static uint8_t *slurp(const char *path, size_t *size)
{
    FILE *f = fopen(path, "rb");
    if (!f) {
        perror(path);
        return NULL;
    }
    fseek(f, 0, SEEK_END);
    long n = ftell(f);
    fseek(f, 0, SEEK_SET);
    uint8_t *buf = malloc((size_t)n);
    if (!buf || fread(buf, 1, (size_t)n, f) != (size_t)n) {
        fprintf(stderr, "%s: short read\n", path);
        free(buf);
        fclose(f);
        return NULL;
    }
    fclose(f);
    *size = (size_t)n;
    return buf;
}

#define MAX_TUS 4096

int main(int argc, char **argv)
{
    if (argc < 3) {
        fprintf(stderr, "usage: %s <stream.obu> <av1C.bin> [luma-dump.gray]\n", argv[0]);
        return 2;
    }

    size_t size = 0, av1c_size = 0;
    uint8_t *data = slurp(argv[1], &size);
    uint8_t *av1c = slurp(argv[2], &av1c_size);
    if (!data || !av1c)
        return 1;

    /* Same requirement VP9 had: without the supplemental registration, the
     * hardware answers "no" even where it exists. */
    VTRegisterSupplementalVideoDecoderIfAvailable(kCMVideoCodecType_AV1);
    printf("VTIsHardwareDecodeSupported(AV1) = %s\n",
           VTIsHardwareDecodeSupported(kCMVideoCodecType_AV1) ? "YES" : "no");
    printf("av1C record                      : %zu bytes\n", av1c_size);

    struct tu *tus = calloc(MAX_TUS, sizeof(*tus));
    int seq_headers = 0;
    int n_tus = split_temporal_units(data, size, tus, MAX_TUS, &seq_headers);
    if (n_tus < 0)
        return 1;

    int frame_obus = 0;
    for (int i = 0; i < n_tus; i++)
        frame_obus += tus[i].frame_obus;
    printf("temporal units in stream         : %d\n", n_tus);
    printf("  frame OBUs across them         : %d\n", frame_obus);
    printf("  sequence headers               : %d\n\n", seq_headers);

    CFDataRef av1c_data = CFDataCreate(kCFAllocatorDefault, av1c, (CFIndex)av1c_size);
    CFMutableDictionaryRef atoms = CFDictionaryCreateMutable(
        kCFAllocatorDefault, 1, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    CFDictionarySetValue(atoms, CFSTR("av1C"), av1c_data);
    CFMutableDictionaryRef ext = CFDictionaryCreateMutable(
        kCFAllocatorDefault, 1, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    CFDictionarySetValue(ext, kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms, atoms);

    CMVideoFormatDescriptionRef fmt = NULL;
    OSStatus st = CMVideoFormatDescriptionCreate(kCFAllocatorDefault, kCMVideoCodecType_AV1,
                                                 0, 0, ext, &fmt);
    if (st != noErr) {
        fprintf(stderr, "CMVideoFormatDescriptionCreate failed: %d\n", (int)st);
        return 1;
    }

    struct decode_stats stats = {0};
    if (argc > 3) {
        stats.dump = fopen(argv[3], "wb");
        if (!stats.dump) {
            perror(argv[3]);
            return 1;
        }
        printf("dumping luma planes to %s\n", argv[3]);
    }
    VTDecompressionOutputCallbackRecord cb = {output_callback, &stats};
    CFMutableDictionaryRef pixel_attrs = CFDictionaryCreateMutable(
        kCFAllocatorDefault, 2, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    CFDictionarySetValue(pixel_attrs, kCVPixelBufferIOSurfacePropertiesKey,
                         CFDictionaryCreate(kCFAllocatorDefault, NULL, NULL, 0,
                                            &kCFTypeDictionaryKeyCallBacks,
                                            &kCFTypeDictionaryValueCallBacks));

    VTDecompressionSessionRef session = NULL;
    st = VTDecompressionSessionCreate(kCFAllocatorDefault, fmt, NULL, pixel_attrs, &cb, &session);
    if (st != noErr) {
        fprintf(stderr, "VTDecompressionSessionCreate failed: %d\n", (int)st);
        return 1;
    }
    printf("session created; submitting %d temporal units\n\n", n_tus);

    int submitted = 0;
    for (int i = 0; i < n_tus; i++) {
        CMBlockBufferRef bb = NULL;
        st = CMBlockBufferCreateWithMemoryBlock(kCFAllocatorDefault, (void *)tus[i].data,
                                               tus[i].size, kCFAllocatorNull, NULL, 0,
                                               tus[i].size, 0, &bb);
        if (st != noErr) {
            fprintf(stderr, "CMBlockBufferCreateWithMemoryBlock failed: %d\n", (int)st);
            break;
        }

        CMSampleBufferRef sb = NULL;
        size_t sample_size = tus[i].size;
        st = CMSampleBufferCreate(kCFAllocatorDefault, bb, TRUE, NULL, NULL, fmt, 1, 0, NULL,
                                  1, &sample_size, &sb);
        if (st != noErr) {
            fprintf(stderr, "CMSampleBufferCreate failed: %d\n", (int)st);
            CFRelease(bb);
            break;
        }

        VTDecodeInfoFlags info = 0;
        stats.in_decode_call = true;
        st = VTDecompressionSessionDecodeFrame(session, sb, 0, NULL, &info);
        stats.in_decode_call = false;
        if (st != noErr) {
            fprintf(stderr, "DecodeFrame(tu %d) failed: %d\n", i, (int)st);
            CFRelease(sb);
            CFRelease(bb);
            break;
        }
        submitted++;
        CFRelease(sb);
        CFRelease(bb);
    }

    VTDecompressionSessionWaitForAsynchronousFrames(session);

    char fourcc[5] = {0};
    uint32_t pf = (uint32_t)stats.pixel_format;
    fourcc[0] = (char)(pf >> 24); fourcc[1] = (char)(pf >> 16);
    fourcc[2] = (char)(pf >> 8);  fourcc[3] = (char)pf;

    printf("submitted to VT                  : %d\n", submitted);
    printf("output callbacks                 : %d\n", stats.outputs);
    printf("  arrived after return           : %d\n", stats.after_return);
    printf("  with real pixels               : %d\n", stats.nonzero);
    printf("  null images                    : %d\n", stats.null_images);
    printf("  errors                         : %d\n", stats.errors);
    printf("pixel format                     : %s  %zux%zu (%d planes)\n",
           fourcc, stats.last_w, stats.last_h, stats.planes);
    printf("callback thread                  : %s\n",
           pthread_equal(stats.cb_thread, pthread_self()) ? "SAME as caller"
                                                          : "DIFFERENT from caller");
    /* Which unit the 1:1 holds in is the whole answer, so name it. A stream in
     * its natural framing bundles a no-show frame and the frame that displays it
     * into ONE temporal unit, and VideoToolbox returns one picture for the pair --
     * TU-level 1:1, which the protocol cannot use. virgl submits one FRAME at a
     * time, so what the backend needs is a picture per frame OBU. Feed each frame
     * its own temporal delimiter (see make-sample.sh's per-frame rewrite) and that
     * is what comes back. */
    printf("\ncardinality: %d TUs / %d frame OBUs in, %d pictures out\n",
           submitted, frame_obus, stats.outputs);
    if (stats.outputs == frame_obus && frame_obus == submitted)
        printf("  -> a picture per FRAME. This is the VP9 property, and what virgl needs.\n");
    else if (stats.outputs == submitted)
        printf("  -> a picture per TEMPORAL UNIT, not per frame: %d frame OBUs produced no\n"
               "     picture of their own. Re-run on a per-frame-framed stream before\n"
               "     concluding anything about the backend.\n", frame_obus - stats.outputs);
    else
        printf("  -> NOT 1:1 in either unit; the backend owes a reorder buffer.\n");

    if (stats.dump)
        fclose(stats.dump);
    VTDecompressionSessionInvalidate(session);
    CFRelease(session);
    CFRelease(fmt);
    free(tus);
    free(data);
    free(av1c);
    return 0;
}
