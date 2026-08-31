/*
 * Does VideoToolbox let an HEVC decompression session survive a change to its SPS?
 *
 * This is the question the whole HEVC design rests on, and it is answerable without a
 * guest, a serializer, or any virglrenderer code. The SPS short_term_ref_pic_set contents
 * are not on the VA-API wire, so the backend has to synthesize them and refine them as it
 * learns which sets a stream actually uses -- which means handing the live session a new
 * SPS mid-stream. If VideoToolbox refuses that, the design is dead, because a new set is
 * first referenced by a frame that still needs the DPB built from the frames before it.
 *
 *   ./probe <clip.265>            decode straight through, print a per-frame digest
 *   ./probe <clip.265> <n>        swap in a modified SPS before frame n and keep going
 *
 * The modification (general_level_idc) is semantically inert, so a session that truly
 * survives the swap must produce a byte-identical digest list. Any difference is the DPB
 * being lost, whatever CanAcceptFormatDescription claimed.
 */
#include <CoreFoundation/CoreFoundation.h>
#include <CoreMedia/CoreMedia.h>
#include <VideoToolbox/VideoToolbox.h>
#include <CoreVideo/CoreVideo.h>
#include <CommonCrypto/CommonDigest.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static uint8_t *slurp(const char *p, size_t *len)
{
    FILE *f = fopen(p, "rb");
    if (!f) { perror(p); exit(1); }
    fseek(f, 0, SEEK_END); long n = ftell(f); fseek(f, 0, SEEK_SET);
    uint8_t *b = malloc((size_t)n);
    if (fread(b, 1, (size_t)n, f) != (size_t)n) { fprintf(stderr, "short read\n"); exit(1); }
    fclose(f); *len = (size_t)n; return b;
}

/* Annex-B walk. Returns the next NAL payload (after the start code) and its length. */
static const uint8_t *next_nal(const uint8_t *p, const uint8_t *end, size_t *out_len)
{
    const uint8_t *s = NULL;
    while (p + 3 <= end) {
        if (p[0] == 0 && p[1] == 0 && (p[2] == 1 || (p + 4 <= end && p[2] == 0 && p[3] == 1))) {
            s = p + (p[2] == 1 ? 3 : 4);
            break;
        }
        p++;
    }
    if (!s) return NULL;
    const uint8_t *q = s;
    while (q + 3 <= end) {
        if (q[0] == 0 && q[1] == 0 && (q[2] == 1 || (q + 4 <= end && q[2] == 0 && q[3] == 1)))
            break;
        q++;
    }
    if (q + 3 > end) q = end;
    *out_len = (size_t)(q - s);
    return s;
}

static size_t unescape(const uint8_t *in, size_t n, uint8_t *out)
{
    size_t o = 0, zeros = 0;
    for (size_t i = 0; i < n; i++) {
        if (zeros == 2 && in[i] == 3) { zeros = 0; continue; }
        out[o++] = in[i];
        zeros = in[i] == 0 ? zeros + 1 : 0;
    }
    return o;
}

static size_t escape(const uint8_t *in, size_t n, uint8_t *out)
{
    size_t o = 0, zeros = 0;
    for (size_t i = 0; i < n; i++) {
        if (zeros == 2 && in[i] <= 3) { out[o++] = 3; zeros = 0; }
        out[o++] = in[i];
        zeros = in[i] == 0 ? zeros + 1 : 0;
    }
    return o;
}

struct frame { unsigned idx; char md5[33]; };
static struct frame frames[4096];
static unsigned nframes;

static void on_frame(void *ref, void *sref, OSStatus st, VTDecodeInfoFlags flags,
                     CVImageBufferRef img, CMTime pts, CMTime dur)
{
    (void)ref; (void)sref; (void)flags; (void)pts; (void)dur;
    if (st != noErr || !img) {
        fprintf(stderr, "  decode callback: status %d, image %p\n", (int)st, (void *)img);
        return;
    }
    CVPixelBufferLockBaseAddress(img, kCVPixelBufferLock_ReadOnly);
    CC_MD5_CTX c; CC_MD5_Init(&c);
    size_t planes = CVPixelBufferGetPlaneCount(img);
    for (size_t p = 0; p < planes; p++) {
        const uint8_t *base = CVPixelBufferGetBaseAddressOfPlane(img, p);
        size_t h = CVPixelBufferGetHeightOfPlane(img, p);
        size_t w = CVPixelBufferGetWidthOfPlane(img, p);
        size_t stride = CVPixelBufferGetBytesPerRowOfPlane(img, p);
        size_t bytes = w * (CVPixelBufferGetPixelFormatType(img) == kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
                            ? (p ? 2 : 1) : (p ? 2 : 1));
        for (size_t y = 0; y < h; y++) CC_MD5_Update(&c, base + y * stride, (CC_LONG)bytes);
    }
    CVPixelBufferUnlockBaseAddress(img, kCVPixelBufferLock_ReadOnly);
    unsigned char d[16]; CC_MD5_Final(d, &c);
    if (nframes < 4096) {
        frames[nframes].idx = nframes;
        for (int i = 0; i < 16; i++) sprintf(frames[nframes].md5 + i * 2, "%02x", d[i]);
        nframes++;
    }
}

int main(int argc, char **argv)
{
    if (argc < 2) { fprintf(stderr, "usage: probe <clip.265> [swap_before_frame]\n"); return 2; }
    long swap_at = argc > 2 ? (strcmp(argv[2], "create") ? strtol(argv[2], NULL, 10) : -2) : -1;
    const char *alt_sps_from = argc > 3 ? argv[3] : NULL;

    size_t len; uint8_t *buf = slurp(argv[1], &len);
    const uint8_t *end = buf + len;

    /* Collect the parameter sets and the access units. */
    static uint8_t vps[512], sps[512], pps[512];
    size_t vps_len = 0, sps_len = 0, pps_len = 0;
    static const uint8_t *au_nal[8192]; static size_t au_len[8192]; static int au_first[8192];
    unsigned nnal = 0;

    const uint8_t *p = buf; size_t nl;
    while ((p = next_nal(p, end, &nl)) != NULL) {
        unsigned type = (p[0] >> 1) & 0x3f;
        if (type == 32 && !vps_len) { memcpy(vps, p, nl); vps_len = nl; }
        else if (type == 33 && !sps_len) { memcpy(sps, p, nl); sps_len = nl; }
        else if (type == 34 && !pps_len) { memcpy(pps, p, nl); pps_len = nl; }
        else if (type <= 31) {
            au_nal[nnal] = p; au_len[nnal] = nl;
            au_first[nnal] = (p[2] & 0x80) != 0;   /* first_slice_segment_in_pic_flag */
            nnal++;
        }
        p += nl;
    }
    if (!vps_len || !sps_len || !pps_len) { fprintf(stderr, "missing parameter sets\n"); return 1; }
    fprintf(stderr, "parameter sets: vps %zu sps %zu pps %zu, %u slice NALs\n",
            vps_len, sps_len, pps_len, nnal);

    /* The modified SPS: general_level_idc lives at RBSP byte 12 when
     * sps_max_sub_layers_minus1 is 0. Inert, and enough to ask the question. */
    static uint8_t sps2[512]; size_t sps2_len = 0;
    {
        static uint8_t rbsp[512];
        size_t rn = unescape(sps, sps_len, rbsp);
        /* rbsp[] still carries the 2-byte NAL header, so:
         *   [2] sps_video_parameter_set_id(4) max_sub_layers_minus1(3) nesting(1)
         *   [3] profile_space(2) tier(1) profile_idc(5)
         *   [4..7] profile_compatibility_flag[32]
         *   [8..13] progressive/interlaced/non_packed/frame_only + 44 reserved bits
         *   [14] general_level_idc
         * Only valid for sps_max_sub_layers_minus1 == 0, which every clip here is. */
        if (rn <= 14) { fprintf(stderr, "SPS too short to edit\n"); return 1; }
        /* Move to a DEFINED level, not just a different byte: levels are 30*x, and an
         * undefined value is its own reason for VideoToolbox to refuse the description --
         * which would look exactly like "SPS changes are refused". */
        unsigned was = rbsp[14];
        rbsp[14] = (uint8_t)(was >= 120 ? 150 : 120);
        sps2_len = escape(rbsp, rn, sps2);
        fprintf(stderr, "modified SPS: general_level_idc %u -> %u, %zu -> %zu bytes\n",
                was, rbsp[14], sps_len, sps2_len);
    }

    /* A genuinely different SPS -- same dimensions, different reference picture sets --
     * asks the two questions at once: does VideoToolbox consult set contents at all, and
     * does the session survive being handed new ones. */
    if (alt_sps_from) {
        size_t alen; uint8_t *abuf = slurp(alt_sps_from, &alen);
        const uint8_t *ae = abuf + alen, *ap = abuf; size_t anl;
        sps2_len = 0;
        while ((ap = next_nal(ap, ae, &anl)) != NULL) {
            if (((ap[0] >> 1) & 0x3f) == 33) { memcpy(sps2, ap, anl); sps2_len = anl; break; }
            ap += anl;
        }
        if (!sps2_len) { fprintf(stderr, "no SPS in %s\n", alt_sps_from); return 1; }
        fprintf(stderr, "swap SPS comes from %s (%zu bytes vs %zu)\n",
                alt_sps_from, sps2_len, sps_len);
    }

    /* "create" builds the session from the alternate SPS instead of swapping to it later:
     * the placeholder sets the serializer will emit are present from frame 0, which is the
     * case a mid-stream swap never exercises. */
    int from_creation = swap_at == -2;
    if (from_creation) { memcpy(sps, sps2, sps2_len); sps_len = sps2_len; swap_at = -1;
                         fprintf(stderr, "using the alternate SPS from session creation\n"); }

    const uint8_t *sets[3] = { vps, sps, pps };
    const size_t set_sizes[3] = { vps_len, sps_len, pps_len };
    CMVideoFormatDescriptionRef fd = NULL, fd2 = NULL;
    OSStatus st = CMVideoFormatDescriptionCreateFromHEVCParameterSets(
        kCFAllocatorDefault, 3, sets, set_sizes, 4, NULL, &fd);
    if (st != noErr) { fprintf(stderr, "FD create failed: %d\n", (int)st); return 1; }

    VTDecompressionOutputCallbackRecord cb = { on_frame, NULL };
    CFMutableDictionaryRef attrs = CFDictionaryCreateMutable(kCFAllocatorDefault, 1,
        &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    int32_t fmt = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;
    CFNumberRef fn = CFNumberCreate(kCFAllocatorDefault, kCFNumberSInt32Type, &fmt);
    CFDictionarySetValue(attrs, kCVPixelBufferPixelFormatTypeKey, fn); CFRelease(fn);

    VTDecompressionSessionRef sess = NULL;
    st = VTDecompressionSessionCreate(kCFAllocatorDefault, fd, NULL, attrs, &cb, &sess);
    CFRelease(attrs);
    if (st != noErr) { fprintf(stderr, "session create failed: %d\n", (int)st); return 1; }

    CFBooleanRef hw = NULL;
    if (VTSessionCopyProperty(sess, kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder,
                              kCFAllocatorDefault, &hw) == noErr && hw) {
        fprintf(stderr, "hardware accelerated: %s\n", CFBooleanGetValue(hw) ? "yes" : "NO");
        CFRelease(hw);
    }

    /* Feed access units, grouping slice NALs by first_slice_segment_in_pic_flag. */
    static uint8_t au[1 << 22]; size_t aulen = 0; unsigned fed = 0;
    for (unsigned i = 0; i <= nnal; i++) {
        int flush = (i == nnal) || (au_first[i] && aulen > 0);
        if (flush && aulen) {
            if (swap_at >= 0 && (long)fed == swap_at) {
                const uint8_t *s2[3] = { vps, sps2, pps };
                const size_t z2[3] = { vps_len, sps2_len, pps_len };
                st = CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                    kCFAllocatorDefault, 3, s2, z2, 4, NULL, &fd2);
                if (st != noErr) { fprintf(stderr, "FD2 create failed: %d\n", (int)st); return 1; }
                Boolean ok = VTDecompressionSessionCanAcceptFormatDescription(sess, fd2);
                fprintf(stderr, ">>> frame %u: CanAcceptFormatDescription(new SPS) = %s\n",
                        fed, ok ? "YES" : "NO");
                fd = fd2;
            }
            CMBlockBufferRef bb = NULL;
            CMBlockBufferCreateWithMemoryBlock(kCFAllocatorDefault, au, aulen, kCFAllocatorNull,
                                               NULL, 0, aulen, 0, &bb);
            CMSampleBufferRef sb = NULL;
            const size_t sz = aulen;
            CMSampleBufferCreateReady(kCFAllocatorDefault, bb, fd, 1, 0, NULL, 1, &sz, &sb);
            VTDecodeInfoFlags out = 0;
            st = VTDecompressionSessionDecodeFrame(sess, sb, 0, NULL, &out);
            if (st != noErr) fprintf(stderr, "  frame %u: decode failed %d\n", fed, (int)st);
            CFRelease(sb); CFRelease(bb);
            fed++; aulen = 0;
        }
        if (i == nnal) break;
        au[aulen++] = 0; au[aulen++] = 0; au[aulen++] = 0; au[aulen++] = 1;
        uint32_t n32 = (uint32_t)au_len[i];
        au[aulen - 4] = (uint8_t)(n32 >> 24); au[aulen - 3] = (uint8_t)(n32 >> 16);
        au[aulen - 2] = (uint8_t)(n32 >> 8);  au[aulen - 1] = (uint8_t)n32;
        memcpy(au + aulen, au_nal[i], au_len[i]); aulen += au_len[i];
    }
    VTDecompressionSessionWaitForAsynchronousFrames(sess);
    fprintf(stderr, "fed %u access units, got %u frames\n", fed, nframes);
    for (unsigned i = 0; i < nframes; i++) printf("%u %s\n", frames[i].idx, frames[i].md5);
    return 0;
}
