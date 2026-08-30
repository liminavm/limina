/*
 * Drive the serializer into VideoToolbox, on real AV1 silicon.
 *
 * The dav1d oracle proves the rebuilt bitstream is correct. It cannot prove the *backend*
 * is: the av1C configuration record, the decode session, one temporal unit per sample, and
 * the order the units are submitted in are all host-side code that dav1d never touches.
 * This runs that half, in the same sequence virgl_video_vt.c uses -- flush the held frame
 * when a descriptor arrives, build the frame's own unit at end of frame -- and reports what
 * VideoToolbox gives back.
 *
 * Needs an M3 or later Mac; earlier silicon has no AV1 decoder and the probe says so rather
 * than reporting a failure that is really an absence.
 *
 * Each picture's planes are checksummed so the result can be compared against the dav1d
 * oracle's `AV1_ORACLE_HASH=1` output from any machine: AV1 is a bit-exact format, so two
 * conforming decoders must agree, and a mismatch is a real defect rather than a tolerance.
 *
 * The two do not lay the planes out the same way. VideoToolbox returns biplanar 4:2:0, so
 * its plane 1 is Cb and Cr interleaved and its sum equals dav1d's plane 1 plus plane 2;
 * luma compares directly. Film grain is the documented exception -- VideoToolbox applies it
 * and dav1d here does not -- so a grain clip is expected to differ, and is reported rather
 * than asserted.
 */
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <dirent.h>

#include <CoreFoundation/CoreFoundation.h>
#include <CoreMedia/CoreMedia.h>
#include <CoreVideo/CoreVideo.h>
#include <VideoToolbox/VideoToolbox.h>

#include "virgl_video_hw.h"
#include "virgl_video_av1_obu.h"

struct picture {
    uint64_t sum[3];
    unsigned order_hint;   /* filled in after the fact, from the submission order */
    bool shown;
    uint32_t width, height;
    unsigned planes;
};

#define MAX_PICS 512
static struct picture pics[MAX_PICS];
static unsigned num_pics;
static unsigned decode_errors;
static unsigned blank_pics;

/* Order hint and shown flag of each frame, in submission order. VideoToolbox returns one
 * picture per submitted unit, in order, so picture i belongs to submitted frame i -- which
 * is what lets these be lined up against a decoder that only emits the shown ones. */
static unsigned sub_hint[MAX_PICS];
static bool sub_shown[MAX_PICS];
static unsigned num_submitted;

/* Sum every byte of every plane. Enough to catch a wrong picture, a stale surface or a
 * plane that never got written, and cheap to compare against another decoder's. */
static void checksum(CVPixelBufferRef pixbuf, struct picture *out)
{
    size_t n = CVPixelBufferGetPlaneCount(pixbuf);

    memset(out, 0, sizeof(*out));
    if (CVPixelBufferLockBaseAddress(pixbuf, kCVPixelBufferLock_ReadOnly) != kCVReturnSuccess)
        return;

    out->width = (uint32_t)CVPixelBufferGetWidth(pixbuf);
    out->height = (uint32_t)CVPixelBufferGetHeight(pixbuf);
    out->planes = (unsigned)(n > 3 ? 3 : n);

    for (unsigned p = 0; p < out->planes; p++) {
        const uint8_t *base = CVPixelBufferGetBaseAddressOfPlane(pixbuf, p);
        size_t pitch = CVPixelBufferGetBytesPerRowOfPlane(pixbuf, p);
        size_t w = CVPixelBufferGetWidthOfPlane(pixbuf, p);
        size_t h = CVPixelBufferGetHeightOfPlane(pixbuf, p);
        /* VideoToolbox hands back biplanar 4:2:0: plane 1 holds Cb and Cr interleaved, two
         * bytes per pixel, so counting `w` bytes a row would read half the chroma and call
         * the rest zero. The row stride is padded, so the width has to drive this, not the
         * pitch. */
        size_t bpp = (out->planes == 2 && p == 1) ? 2 : 1;
        uint64_t s = 0;

        for (size_t y = 0; y < h; y++)
            for (size_t x = 0; x < w * bpp; x++)
                s += base[y * pitch + x];
        out->sum[p] = s;
    }
    CVPixelBufferUnlockBaseAddress(pixbuf, kCVPixelBufferLock_ReadOnly);
}

static void on_picture(void *ctx, void *frame_ref, OSStatus status,
                       VTDecodeInfoFlags flags, CVImageBufferRef image,
                       CMTime pts, CMTime dur)
{
    (void)ctx; (void)frame_ref; (void)flags; (void)pts; (void)dur;

    if (status != noErr) {
        fprintf(stderr, "  decode error: status %d\n", (int)status);
        decode_errors++;
        return;
    }
    if (!image) {
        fprintf(stderr, "  decode returned no image\n");
        decode_errors++;
        return;
    }
    if (num_pics >= MAX_PICS)
        return;

    checksum((CVPixelBufferRef)image, &pics[num_pics]);
    if (!pics[num_pics].sum[0])
        blank_pics++;
    num_pics++;
}

static VTDecompressionSessionRef session;
static CMVideoFormatDescriptionRef format;

static int make_session(const uint8_t *av1c, size_t av1c_len, uint32_t w, uint32_t h)
{
    CFDataRef data = CFDataCreate(kCFAllocatorDefault, av1c, (CFIndex)av1c_len);
    CFMutableDictionaryRef atoms, ext;
    VTDecompressionOutputCallbackRecord cb = { on_picture, NULL };
    OSStatus status;

    atoms = CFDictionaryCreateMutable(kCFAllocatorDefault, 1, &kCFTypeDictionaryKeyCallBacks,
                                      &kCFTypeDictionaryValueCallBacks);
    CFDictionarySetValue(atoms, CFSTR("av1C"), data);
    CFRelease(data);
    ext = CFDictionaryCreateMutable(kCFAllocatorDefault, 1, &kCFTypeDictionaryKeyCallBacks,
                                    &kCFTypeDictionaryValueCallBacks);
    CFDictionarySetValue(ext, kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms,
                         atoms);
    CFRelease(atoms);

    status = CMVideoFormatDescriptionCreate(kCFAllocatorDefault, kCMVideoCodecType_AV1,
                                            w, h, ext, &format);
    CFRelease(ext);
    if (status != noErr) {
        fprintf(stderr, "CMVideoFormatDescriptionCreate failed: %d\n", (int)status);
        return -1;
    }

    status = VTDecompressionSessionCreate(kCFAllocatorDefault, format, NULL, NULL, &cb,
                                          &session);
    if (status != noErr) {
        fprintf(stderr, "VTDecompressionSessionCreate failed: %d\n", (int)status);
        return -1;
    }
    return 0;
}

/* One unit, one sample -- the property the backend depends on. */
static int submit_unit(const uint8_t *data, size_t len)
{
    CMBlockBufferRef block = NULL;
    CMSampleBufferRef sample = NULL;
    VTDecodeInfoFlags info = 0;
    OSStatus status;

    if (!len)
        return 0;

    status = CMBlockBufferCreateWithMemoryBlock(kCFAllocatorDefault, (void *)(uintptr_t)data,
                                                len, kCFAllocatorNull, NULL, 0, len, 0, &block);
    if (status != noErr) {
        fprintf(stderr, "CMBlockBufferCreateWithMemoryBlock failed: %d\n", (int)status);
        return -1;
    }
    status = CMSampleBufferCreate(kCFAllocatorDefault, block, TRUE, NULL, NULL, format,
                                  1, 0, NULL, 1, &len, &sample);
    if (status != noErr) {
        fprintf(stderr, "CMSampleBufferCreate failed: %d\n", (int)status);
        CFRelease(block);
        return -1;
    }
    status = VTDecompressionSessionDecodeFrame(session, sample, 0, NULL, &info);
    CFRelease(sample);
    CFRelease(block);
    if (status != noErr) {
        fprintf(stderr, "DecodeFrame failed: %d\n", (int)status);
        return -1;
    }
    return 0;
}

static void *slurp(const char *path, size_t *len)
{
    FILE *f = fopen(path, "rb");
    void *buf;
    long n;

    if (!f)
        return NULL;
    fseek(f, 0, SEEK_END);
    n = ftell(f);
    fseek(f, 0, SEEK_SET);
    buf = malloc((size_t)n ? (size_t)n : 1);
    if (buf && n && fread(buf, 1, (size_t)n, f) != (size_t)n) {
        free(buf);
        buf = NULL;
    }
    fclose(f);
    *len = (size_t)n;
    return buf;
}

int main(int argc, char **argv)
{
    struct virgl_av1_obu_state state;
    uint8_t *unit = NULL;
    size_t unit_cap = 0;
    unsigned frames = 0, held = 0, units = 0, held_hint = 0;
    uint8_t av1c[256];
    ssize_t av1c_len = -1;

    if (argc < 2) {
        fprintf(stderr, "usage: %s <capture-dir>\n\n"
                "  capture-dir  frameNNNNN.desc/.tile from a LIMINA_AV1_CAPTURE run\n", argv[0]);
        return 2;
    }

    if (!VTIsHardwareDecodeSupported(kCMVideoCodecType_AV1)) {
        printf("SKIP: this machine has no AV1 hardware decoder (needs M3 or later)\n");
        return 0;
    }

    virgl_av1_obu_state_init(&state);

    for (unsigned i = 0; ; i++) {
        char path[1024];
        struct virgl_av1_picture_desc desc;
        size_t dlen = 0, tlen = 0;
        void *draw, *tiles;
        ssize_t n;

        snprintf(path, sizeof(path), "%s/frame%05u.desc", argv[1], i);
        draw = slurp(path, &dlen);
        if (!draw)
            break;
        if (dlen != sizeof(desc)) {
            fprintf(stderr, "frame %u: descriptor is %zu bytes, expected %zu -- the capture "
                    "was made by a different build\n", i, dlen, sizeof(desc));
            return 1;
        }
        memcpy(&desc, draw, sizeof(desc));
        free(draw);

        snprintf(path, sizeof(path), "%s/frame%05u.tile", argv[1], i);
        tiles = slurp(path, &tlen);

        if (unit_cap < tlen + VIRGL_AV1_UNIT_OVERHEAD * 2) {
            unit_cap = tlen + VIRGL_AV1_UNIT_OVERHEAD * 2;
            unit = realloc(unit, unit_cap);
        }

        /* The session is built from the first frame's av1C, exactly as the backend does. */
        if (av1c_len < 0) {
            av1c_len = virgl_av1_build_av1c(&state, &desc, av1c, sizeof(av1c));
            if (av1c_len < 0) {
                fprintf(stderr, "could not build the av1C record\n");
                return 1;
            }
            printf("av1C record: %zd bytes\n", av1c_len);
            if (make_session(av1c, (size_t)av1c_len,
                             desc.picture_parameter.frame_width,
                             desc.picture_parameter.frame_height) < 0)
                return 1;
        }

        /* decode_bitstream: flush whatever is held, now that a descriptor settles it. */
        n = virgl_av1_flush_held(&state, &desc, unit, unit_cap);
        if (n < 0) {
            fprintf(stderr, "frame %u: flush refused\n", i);
            return 1;
        }
        if (n > 0) {
            if (num_submitted < MAX_PICS) {
                sub_hint[num_submitted] = held_hint;
                sub_shown[num_submitted++] = false;
            }
            if (submit_unit(unit, (size_t)n) < 0)
                return 1;
            units++;
        }

        /* end_frame: build this frame's own unit. */
        n = virgl_av1_build_temporal_unit(&state, &desc, tiles, tlen, unit, unit_cap);
        free(tiles);
        if (n < 0) {
            fprintf(stderr, "frame %u: build refused\n", i);
            return 1;
        }
        if (!n) {
            held++;
            held_hint = desc.picture_parameter.order_hint;
        } else {
            if (num_submitted < MAX_PICS) {
                sub_hint[num_submitted] = desc.picture_parameter.order_hint;
                sub_shown[num_submitted++] = desc.picture_parameter.pic_info_fields.show_frame;
            }
            if (submit_unit(unit, (size_t)n) < 0)
                return 1;
            units++;
        }
        frames++;
    }

    /* Anything still held at the end of the stream. */
    {
        ssize_t n = virgl_av1_flush_temporal_unit(&state, unit, unit_cap);

        if (n > 0) {
            if (submit_unit(unit, (size_t)n) < 0)
                return 1;
            units++;
        }
    }

    VTDecompressionSessionWaitForAsynchronousFrames(session);

    printf("\nsubmitted %u frames as %u temporal units (%u were held a submission)\n",
           frames, units, held);
    printf("VideoToolbox returned %u pictures, %u errors, %u blank\n",
           num_pics, decode_errors, blank_pics);

    /* Sizes, not just the first: a decoder that declines to upscale a superres frame hands
     * back the coded width instead of the upscaled one, which shows up here and nowhere
     * else. */
    printf("picture sizes returned:\n");
    for (unsigned i = 0; i < num_pics; i++) {
        bool seen = false;
        for (unsigned k = 0; k < i && !seen; k++)
            seen = pics[k].width == pics[i].width && pics[k].height == pics[i].height;
        if (!seen) {
            unsigned count = 0;
            for (unsigned k = 0; k < num_pics; k++)
                if (pics[k].width == pics[i].width && pics[k].height == pics[i].height)
                    count++;
            printf("  %ux%u  x%u\n", pics[i].width, pics[i].height, count);
        }
    }

    /* Keyed on order hint, not position: the dav1d oracle emits only the frames the guest
     * shows, while VideoToolbox returns one for every frame decoded. */
    printf("\nplane checksums, shown frames only (luma, chroma) -- compare against the "
           "dav1d oracle's AV1_ORACLE_HASH=1:\n");
    for (unsigned i = 0; i < num_pics && i < num_submitted; i++)
        if (sub_shown[i])
            printf("  oh=%-3u %016llx %016llx\n", sub_hint[i],
                   (unsigned long long)pics[i].sum[0],
                   (unsigned long long)pics[i].sum[1]);

    virgl_av1_obu_state_fini(&state);
    free(unit);

    if (decode_errors || blank_pics || units != frames) {
        printf("\nFAIL: %u errors, %u blank pictures, %u units for %u frames\n",
               decode_errors, blank_pics, units, frames);
        return 1;
    }
    printf("\nPASS: every frame decoded to a picture with real pixels\n");
    return 0;
}
