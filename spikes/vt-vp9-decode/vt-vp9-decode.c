// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva
//
// Can VideoToolbox stand in for a VA-API VP9 decoder?
//
// The virgl video protocol hands the host one VP9 frame at a time — the guest's
// ffmpeg/GStreamer has already split superframes — and expects a decoded picture
// back for EVERY frame, including the hidden alt-ref frames (show_frame=0) that a
// later show_existing_frame will display. A backend that silently drops those is
// useless, so this asks VideoToolbox directly, before any of it is wired into
// virglrenderer.
//
//   cc -O2 -o vt-vp9-decode vt-vp9-decode.c \
//      -framework VideoToolbox -framework CoreMedia -framework CoreVideo -framework CoreFoundation
//   ./vt-vp9-decode sample-altref.ivf
//
// Input is IVF because it gives frame boundaries for free. Superframe splitting
// here mirrors what the guest-side decoder does before it ever reaches VA-API.

#include <CoreFoundation/CoreFoundation.h>
#include <CoreMedia/CoreMedia.h>
#include <CoreVideo/CoreVideo.h>
#include <VideoToolbox/VideoToolbox.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <pthread.h>

struct frame_info {
    uint8_t profile;
    bool show_existing_frame;
    bool keyframe;
    bool show_frame;
};

/* Just enough of the VP9 uncompressed header to know what kind of frame this is.
 * Everything past show_frame is irrelevant to us — VideoToolbox parses the rest
 * itself, which is the entire reason this backend is cheap compared to VA-API. */
struct bitreader {
    const uint8_t *data;
    size_t bits;
    size_t pos;
};

static unsigned read_bit(struct bitreader *br)
{
    if (br->pos >= br->bits)
        return 0;
    unsigned b = (br->data[br->pos / 8] >> (7 - (br->pos % 8))) & 1;
    br->pos++;
    return b;
}

static bool parse_frame_info(const uint8_t *data, size_t size, struct frame_info *out)
{
    if (size < 1)
        return false;

    struct bitreader br = {data, size * 8, 0};

    unsigned marker = (read_bit(&br) << 1) | read_bit(&br);
    if (marker != 2)
        return false; /* not a VP9 frame marker */

    unsigned low = read_bit(&br);
    unsigned high = read_bit(&br);
    out->profile = (uint8_t)((high << 1) | low);
    if (out->profile == 3)
        read_bit(&br); /* reserved_zero */

    out->show_existing_frame = read_bit(&br);
    if (out->show_existing_frame) {
        out->keyframe = false;
        out->show_frame = true;
        return true;
    }

    out->keyframe = !read_bit(&br); /* frame_type: 0 = KEY */
    out->show_frame = read_bit(&br);
    return true;
}

/* A VP9 superframe packs several frames plus a trailing index. The guest splits
 * these before VA-API sees them, so we do the same and feed VideoToolbox one
 * frame at a time. Returns the number of sub-frames found. */
static int split_superframe(const uint8_t *data, size_t size,
                            const uint8_t **frames, size_t *sizes, int max)
{
    if (size < 1) {
        return 0;
    }

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

/* The vpcC box VideoToolbox wants in the format description's extension atoms:
 * a 4-byte FullBox header then the VP9 codec configuration record. */
static CFDataRef make_vpcc(uint8_t profile, uint8_t level, uint8_t bit_depth,
                           uint8_t chroma_subsampling, bool full_range)
{
    uint8_t box[12] = {0};
    box[0] = 1; /* version */
    /* box[1..3] = flags, zero */
    box[4] = profile;
    box[5] = level;
    box[6] = (uint8_t)((bit_depth << 4) | (chroma_subsampling << 1) | (full_range ? 1 : 0));
    box[7] = 2;  /* colourPrimaries: unspecified */
    box[8] = 2;  /* transferCharacteristics: unspecified */
    box[9] = 2;  /* matrixCoefficients: unspecified */
    /* box[10..11] = codecInitializationDataSize = 0 */
    return CFDataCreate(kCFAllocatorDefault, box, sizeof(box));
}

struct decode_stats {
    int outputs;
    int null_images;
    int errors;
    int nonzero;
    OSType pixel_format;
    int planes;
    pthread_t cb_thread;
    size_t last_w, last_h;
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

    /* "It returned a buffer" is not "it decoded anything" — read the luma. */
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
    CVPixelBufferUnlockBaseAddress(image_buffer, kCVPixelBufferLock_ReadOnly);
    if (varies)
        st->nonzero++;
}

int main(int argc, char **argv)
{
    if (argc < 2) {
        fprintf(stderr, "usage: %s <file.ivf>\n", argv[0]);
        return 2;
    }

    FILE *f = fopen(argv[1], "rb");
    if (!f) {
        perror("open");
        return 1;
    }
    uint8_t ivf_hdr[32];
    if (fread(ivf_hdr, 1, 32, f) != 32 || memcmp(ivf_hdr, "DKIF", 4) != 0) {
        fprintf(stderr, "not an IVF file\n");
        return 1;
    }
    unsigned width = ivf_hdr[12] | (ivf_hdr[13] << 8);
    unsigned height = ivf_hdr[14] | (ivf_hdr[15] << 8);
    printf("IVF %ux%u, fourcc %.4s\n", width, height, ivf_hdr + 8);

    VTRegisterSupplementalVideoDecoderIfAvailable(kCMVideoCodecType_VP9);
    printf("VTIsHardwareDecodeSupported(VP9) = %s\n\n",
           VTIsHardwareDecodeSupported(kCMVideoCodecType_VP9) ? "YES" : "no");

    /* Profile 0, 8-bit 4:2:0, studio range. Level is advisory here; 4.1 covers
     * anything this spike encodes. */
    CFDataRef vpcc = make_vpcc(0, 41, 8, 1, false);
    CFMutableDictionaryRef atoms = CFDictionaryCreateMutable(
        kCFAllocatorDefault, 1, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    CFDictionarySetValue(atoms, CFSTR("vpcC"), vpcc);
    CFMutableDictionaryRef ext = CFDictionaryCreateMutable(
        kCFAllocatorDefault, 1, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    CFDictionarySetValue(ext, kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms, atoms);

    CMVideoFormatDescriptionRef fmt = NULL;
    OSStatus st = CMVideoFormatDescriptionCreate(kCFAllocatorDefault, kCMVideoCodecType_VP9,
                                                 width, height, ext, &fmt);
    if (st != noErr) {
        fprintf(stderr, "CMVideoFormatDescriptionCreate failed: %d\n", (int)st);
        return 1;
    }

    struct decode_stats stats = {0};
    VTDecompressionOutputCallbackRecord cb = {output_callback, &stats};
    CFMutableDictionaryRef pixel_attrs = CFDictionaryCreateMutable(
        kCFAllocatorDefault, 2, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    int32_t pixfmt = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;
    if (argc > 2 && strcmp(argv[2], "i420") == 0)
        pixfmt = kCVPixelFormatType_420YpCbCr8Planar;
    if (argc > 2 && strcmp(argv[2], "nv12f") == 0)
        pixfmt = kCVPixelFormatType_420YpCbCr8BiPlanarFullRange;
    CFNumberRef pixfmt_num = CFNumberCreate(kCFAllocatorDefault, kCFNumberSInt32Type, &pixfmt);
    CFDictionarySetValue(pixel_attrs, kCVPixelBufferPixelFormatTypeKey, pixfmt_num);
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
    printf("decompression session created\n\n");

    int packets = 0, subframes = 0, hidden = 0, show_existing = 0, submitted = 0;
    int deferred = 0; /* outputs that did NOT arrive before DecodeFrame returned */
    uint8_t *packet = NULL;
    size_t packet_cap = 0;

    for (;;) {
        uint8_t fh[12];
        if (fread(fh, 1, 12, f) != 12)
            break;
        size_t sz = (size_t)fh[0] | ((size_t)fh[1] << 8) | ((size_t)fh[2] << 16) | ((size_t)fh[3] << 24);
        if (sz > packet_cap) {
            packet = realloc(packet, sz);
            packet_cap = sz;
        }
        if (fread(packet, 1, sz, f) != sz)
            break;
        packets++;

        const uint8_t *frames[8];
        size_t sizes[8];
        int n = split_superframe(packet, sz, frames, sizes, 8);
        for (int i = 0; i < n; i++) {
            subframes++;
            struct frame_info fi = {0};
            if (!parse_frame_info(frames[i], sizes[i], &fi)) {
                fprintf(stderr, "  frame %d: bad header\n", subframes);
                continue;
            }
            if (fi.show_existing_frame) {
                /* The guest's decoder re-presents an already-decoded surface for
                 * these; nothing is ever handed to the VA driver. */
                show_existing++;
                continue;
            }
            if (!fi.show_frame)
                hidden++;

            CMBlockBufferRef bb = NULL;
            st = CMBlockBufferCreateWithMemoryBlock(kCFAllocatorDefault, (void *)frames[i],
                                                    sizes[i], kCFAllocatorNull, NULL, 0,
                                                    sizes[i], 0, &bb);
            if (st != noErr) {
                fprintf(stderr, "  block buffer failed: %d\n", (int)st);
                continue;
            }
            CMSampleBufferRef sb = NULL;
            const size_t sample_size = sizes[i];
            st = CMSampleBufferCreate(kCFAllocatorDefault, bb, TRUE, NULL, NULL, fmt, 1, 0, NULL,
                                      1, &sample_size, &sb);
            if (st != noErr) {
                fprintf(stderr, "  sample buffer failed: %d\n", (int)st);
                CFRelease(bb);
                continue;
            }

            VTDecodeInfoFlags info = 0;
            int before = stats.outputs;
            st = VTDecompressionSessionDecodeFrame(session, sb, 0, NULL, &info);
            if (st == noErr && stats.outputs == before)
                deferred++;
            if (st != noErr)
                fprintf(stderr, "  DecodeFrame frame %d (show_frame=%d) failed: %d\n", subframes,
                        fi.show_frame, (int)st);
            else
                submitted++;

            CFRelease(sb);
            CFRelease(bb);
        }
    }
    fclose(f);
    VTDecompressionSessionWaitForAsynchronousFrames(session);

    printf("IVF packets            : %d\n", packets);
    printf("sub-frames after split : %d\n", subframes);
    printf("  show_existing_frame  : %d  (never reaches a VA driver)\n", show_existing);
    printf("  hidden (show_frame=0): %d\n", hidden);
    printf("submitted to VT        : %d\n", submitted);
    printf("output callbacks       : %d\n", stats.outputs);
    printf("  arrived after return : %d  (0 = fully synchronous)\n", deferred);
    printf("  with real pixels     : %d\n", stats.nonzero);
    printf("  NULL image buffers   : %d\n", stats.null_images);
    printf("  errors               : %d\n", stats.errors);
    printf("callback thread        : %s\n",
           pthread_equal(stats.cb_thread, pthread_self()) ? "SAME as caller"
                                                          : "DIFFERENT from caller");
    printf("planes                 : %d\n", stats.planes);
    printf("pixel format           : %.4s  %zux%zu\n", (char *)&(OSType){CFSwapInt32HostToBig(stats.pixel_format)},
           stats.last_w, stats.last_h);

    bool ok = submitted > 0 && stats.outputs == submitted && stats.errors == 0 &&
              stats.nonzero == stats.outputs && deferred == 0;
    printf("\nVERDICT: %s\n", ok ? "every submitted frame came back decoded, hidden ones included"
                                 : "MISMATCH — see counts above");

    VTDecompressionSessionInvalidate(session);
    return ok ? 0 : 1;
}
