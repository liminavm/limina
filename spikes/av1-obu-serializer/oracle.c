/* SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
 * Copyright © 2026 Gustavo Noronha Silva
 *
 * Does our synthesized AV1 frame header decode to the same pixels as the real one?
 *
 * The serializer rebuilds a frame header from the parsed picture descriptor the guest
 * hands the host, because the encoder's own header was destroyed at the guest's
 * decoder -> VA-API boundary. Nothing about that is checkable by inspection: a header is
 * a bit-packed structure whose fields shift each other, so a single wrong value moves
 * everything after it and the failure surfaces as noise somewhere else entirely.
 *
 * dav1d settles it. It is a real decoder, so it judges *conformance* rather than whether
 * fields survived a round-trip -- and it is pure software, so this runs on a machine with
 * no AV1 silicon, which is what keeps the serializer off the M3 critical path.
 *
 * Two decodes, compared frame by frame:
 *   reference -- dav1d over the original clip
 *   subject   -- dav1d over the stream we rebuild from the captured descriptors
 *
 * Pixels are the verdict. Dav1dPicture.frame_hdr is public, so when they disagree the
 * header fields are printed side by side to say *which* field moved, rather than leaving
 * a pixel diff to be reverse-engineered.
 */

#include <dirent.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <dav1d/dav1d.h>

#include "virgl_video_hw.h"
#include "virgl_video_av1_obu.h"

#define MAX_FRAMES 4096

struct picture_list {
    Dav1dPicture pics[MAX_FRAMES];
    unsigned n;
};

static void free_data(const uint8_t *buf, void *cookie) { (void)buf; (void)cookie; }

static int decode_stream(const uint8_t *data, size_t size, struct picture_list *out,
                         const char *what)
{
    Dav1dSettings s;
    Dav1dContext *c = NULL;
    Dav1dData in = {0};
    int err;

    dav1d_default_settings(&s);
    /* Single-threaded and in-order: the comparison is about bits, not throughput, and
     * frame threading would only make the failure harder to localise. */
    s.n_threads = 1;
    s.max_frame_delay = 1;

    if ((err = dav1d_open(&c, &s))) {
        fprintf(stderr, "%s: dav1d_open failed (%d)\n", what, err);
        return -1;
    }

    if (dav1d_data_wrap(&in, data, size, free_data, NULL)) {
        fprintf(stderr, "%s: dav1d_data_wrap failed\n", what);
        dav1d_close(&c);
        return -1;
    }

    do {
        if (in.sz) {
            err = dav1d_send_data(c, &in);
            if (err < 0 && err != DAV1D_ERR(EAGAIN)) {
                fprintf(stderr, "%s: dav1d_send_data failed (%d) with %zu bytes left\n",
                        what, err, in.sz);
                dav1d_data_unref(&in);
                dav1d_close(&c);
                return -1;
            }
        }

        while (1) {
            Dav1dPicture p = {0};

            err = dav1d_get_picture(c, &p);
            if (err == DAV1D_ERR(EAGAIN))
                break;
            if (err < 0) {
                fprintf(stderr, "%s: dav1d_get_picture failed (%d)\n", what, err);
                dav1d_data_unref(&in);
                dav1d_close(&c);
                return -1;
            }
            if (out->n < MAX_FRAMES)
                out->pics[out->n++] = p;
            else
                dav1d_picture_unref(&p);
        }
    } while (in.sz);

    /* Drain. */
    while (1) {
        Dav1dPicture p = {0};

        err = dav1d_get_picture(c, &p);
        if (err == DAV1D_ERR(EAGAIN) || err == DAV1D_ERR(ENOENT))
            break;
        if (err < 0)
            break;
        if (out->n < MAX_FRAMES)
            out->pics[out->n++] = p;
        else
            dav1d_picture_unref(&p);
    }

    dav1d_data_unref(&in);
    dav1d_close(&c);
    return 0;
}

static bool planes_equal(const Dav1dPicture *a, const Dav1dPicture *b, const char **why)
{
    if (a->p.w != b->p.w || a->p.h != b->p.h) { *why = "dimensions"; return false; }
    if (a->p.layout != b->p.layout)           { *why = "layout";     return false; }
    if (a->p.bpc != b->p.bpc)                 { *why = "bit depth";  return false; }

    for (int pl = 0; pl < (a->p.layout == DAV1D_PIXEL_LAYOUT_I400 ? 1 : 3); pl++) {
        const int ss_ver = pl && a->p.layout == DAV1D_PIXEL_LAYOUT_I420;
        const int ss_hor = pl && a->p.layout != DAV1D_PIXEL_LAYOUT_I444;
        const int w = (a->p.w + ss_hor) >> ss_hor;
        const int h = (a->p.h + ss_ver) >> ss_ver;
        const int bytes = a->p.bpc > 8 ? 2 : 1;

        for (int y = 0; y < h; y++) {
            const uint8_t *ra = (const uint8_t *)a->data[pl] + (ptrdiff_t)y * a->stride[!!pl];
            const uint8_t *rb = (const uint8_t *)b->data[pl] + (ptrdiff_t)y * b->stride[!!pl];

            if (memcmp(ra, rb, (size_t)w * bytes)) {
                static char buf[64];
                snprintf(buf, sizeof(buf), "plane %d row %d", pl, y);
                *why = buf;
                return false;
            }
        }
    }
    return true;
}

#define CMP(field) \
    do { if (a->field != b->field) \
            printf("      %-28s reference %-12lld subject %lld\n", #field, \
                   (long long)a->field, (long long)b->field); } while (0)

static void diff_headers(const Dav1dFrameHeader *a, const Dav1dFrameHeader *b)
{
    if (!a || !b) {
        printf("      (a frame header is missing, so no field diff is possible)\n");
        return;
    }
    printf("    frame header fields that differ:\n");
    CMP(frame_type);
    CMP(width[0]);
    CMP(height);
    CMP(frame_offset);
    CMP(primary_ref_frame);
    CMP(refresh_frame_flags);
    CMP(show_frame);
    CMP(showable_frame);
    CMP(error_resilient_mode);
    CMP(disable_cdf_update);
    CMP(allow_screen_content_tools);
    CMP(force_integer_mv);
    CMP(hp);
    CMP(use_ref_frame_mvs);
    CMP(allow_intrabc);
    CMP(warp_motion);
    CMP(reduced_txtp_set);
    CMP(switchable_comp_refs);
    CMP(skip_mode_enabled);
    CMP(subpel_filter_mode);
    CMP(txfm_mode);
    CMP(tiling.cols);
    CMP(tiling.rows);
    CMP(quant.yac);
    CMP(quant.ydc_delta);
    CMP(quant.udc_delta);
    CMP(quant.uac_delta);
    CMP(quant.vdc_delta);
    CMP(quant.vac_delta);
    CMP(segmentation.enabled);
    CMP(segmentation.update_map);
    CMP(segmentation.update_data);
    CMP(loopfilter.level_y[0]);
    CMP(loopfilter.level_y[1]);
    CMP(loopfilter.level_u);
    CMP(loopfilter.level_v);
    CMP(loopfilter.mode_ref_delta_enabled);
    CMP(cdef.damping);
    CMP(cdef.n_bits);
    CMP(restoration.type[0]);
    CMP(restoration.type[1]);
    CMP(restoration.type[2]);
    CMP(super_res.enabled);
    CMP(super_res.width_scale_denominator);
    CMP(film_grain.present);
    for (int i = 0; i < 7; i++) {
        if (a->gmv[i].type != b->gmv[i].type)
            printf("      gmv[%d].type                 reference %-12d subject %d\n",
                   i, a->gmv[i].type, b->gmv[i].type);
        for (int j = 0; j < 6; j++)
            if (a->gmv[i].matrix[j] != b->gmv[i].matrix[j])
                printf("      gmv[%d].matrix[%d]            reference %-12d subject %d\n",
                       i, j, a->gmv[i].matrix[j], b->gmv[i].matrix[j]);
    }
}

static uint8_t *slurp(const char *path, size_t *size)
{
    FILE *f = fopen(path, "rb");
    uint8_t *buf;
    long n;

    if (!f)
        return NULL;
    fseek(f, 0, SEEK_END);
    n = ftell(f);
    fseek(f, 0, SEEK_SET);
    if (n < 0) { fclose(f); return NULL; }
    buf = malloc((size_t)n + 1);
    if (!buf) { fclose(f); return NULL; }
    if (fread(buf, 1, (size_t)n, f) != (size_t)n) { free(buf); fclose(f); return NULL; }
    fclose(f);
    *size = (size_t)n;
    return buf;
}

int main(int argc, char **argv)
{
    struct virgl_av1_obu_state state;
    struct picture_list ref = {0}, sub = {0};
    uint8_t *original, *stream;
    size_t original_size, stream_len = 0, stream_cap = 1 << 22;
    unsigned frames = 0, mismatches = 0, compared = 0;
    /* AV1_ORACLE_CONTRACT=1 drives the serializer the way a buggy backend would -- building
     * each frame without ever flushing the held one -- and checks that it REFUSES rather
     * than quietly losing a picture. Worth a mode of its own because the normal path always
     * flushes, so nothing else here ever reaches that guard. */
    const bool contract = getenv("AV1_ORACLE_CONTRACT") != NULL;
    bool held_seen = false;
    const char *capture_dir, *clip_path;

    if (argc != 3) {
        fprintf(stderr,
                "usage: %s <capture-dir> <original.obu>\n\n"
                "  capture-dir   frameNNNNN.desc/.tile written by a LIMINA_AV1_CAPTURE run\n"
                "  original.obu  the same clip, low-overhead OBU framing\n", argv[0]);
        return 2;
    }
    capture_dir = argv[1];
    clip_path = argv[2];

    if (!(original = slurp(clip_path, &original_size))) {
        fprintf(stderr, "cannot read %s\n", clip_path);
        return 1;
    }

    if (!(stream = malloc(stream_cap))) return 1;

    virgl_av1_obu_state_init(&state);

    for (unsigned i = 0; ; i++) {
        struct virgl_av1_picture_desc desc;
        char path[2048];
        uint8_t *tiles;
        size_t tiles_size = 0;
        ssize_t n;
        FILE *f;

        snprintf(path, sizeof(path), "%s/frame%05u.desc", capture_dir, i);
        if (!(f = fopen(path, "rb")))
            break;
        if (fread(&desc, 1, sizeof(desc), f) != sizeof(desc)) {
            fprintf(stderr, "%s: short read -- built against a different "
                    "virgl_video_hw.h?\n", path);
            fclose(f);
            return 1;
        }
        fclose(f);

        snprintf(path, sizeof(path), "%s/frame%05u.tile", capture_dir, i);
        tiles = slurp(path, &tiles_size);

        if (stream_len + (2 << 20) > stream_cap) {
            stream_cap *= 2;
            stream = realloc(stream, stream_cap);
            if (!stream) return 1;
        }

        /* The order the backend uses, and for the same reasons: the held frame is flushed
         * as soon as the descriptor is known (the guest's first decode_bitstream), and this
         * frame's own unit is built at end_frame once all its tile data has arrived. Each
         * call's output is a separate temporal unit and reaches the decoder as its own
         * sample -- they are adjacent here only because dav1d takes a byte stream. */
        if (contract) {
            n = virgl_av1_build_temporal_unit(&state, &desc, tiles, tiles_size,
                                              stream + stream_len, stream_cap - stream_len);
            free(tiles);
            if (n < 0) {
                if (held_seen) {
                    printf("PASS: the serializer refused to build over a held frame\n");
                    return 0;
                }
                printf("FAIL: refused at frame %u with nothing held\n", i);
                return 1;
            }
            if (!n)
                held_seen = true;
            else if (held_seen) {
                printf("FAIL: frame %u built over a held frame instead of refusing -- the "
                       "held picture is lost\n", i);
                return 1;
            }
            stream_len += (size_t)n;
            frames++;
            continue;
        }

        for (int call = 0; call < 2; call++) {
            /* Twice on purpose. A frame's tile data can arrive over several
             * decode_bitstream calls, each carrying the same descriptor, so the flush has
             * to be idempotent within a frame -- a second unit here would be a second
             * picture the guest never asked for. */
            n = virgl_av1_flush_held(&state, &desc, stream + stream_len,
                                     stream_cap - stream_len);
            if (n < 0) {
                fprintf(stderr, "frame %u: the serializer refused to flush the held frame\n", i);
                return 1;
            }
            if (call && n) {
                fprintf(stderr, "frame %u: flushing twice emitted a second temporal unit "
                        "(%zd bytes)\n", i, n);
                return 1;
            }
            stream_len += (size_t)n;
        }

        n = virgl_av1_build_temporal_unit(&state, &desc, tiles, tiles_size,
                                          stream + stream_len, stream_cap - stream_len);
        free(tiles);
        if (n < 0) {
            fprintf(stderr, "frame %u: the serializer refused to build a temporal unit\n", i);
            return 1;
        }
        stream_len += (size_t)n;
        frames++;
    }

    /* A hidden frame may still be held: it waits one submission so its refresh can be
     * derived from the next descriptor, and after the last one there is no next. */
    if (stream_len + (2 << 20) > stream_cap) {
        stream_cap = stream_len + (2 << 20);
        stream = realloc(stream, stream_cap);
        if (!stream) return 1;
    }
    ssize_t tail = virgl_av1_flush_temporal_unit(&state, stream + stream_len,
                                                 stream_cap - stream_len);
    if (tail < 0) {
        fprintf(stderr, "the serializer refused to flush the held frame\n");
        return 1;
    }
    stream_len += (size_t)tail;

    if (contract) {
        printf("SKIP: no frame was ever held, so the guard was not reached\n");
        return 0;
    }

    if (!frames) {
        fprintf(stderr, "no fixtures in %s\n", capture_dir);
        return 1;
    }

    printf("rebuilt %u frames into %zu bytes (original clip: %zu)\n\n",
           frames, stream_len, original_size);

    if (getenv("AV1_ORACLE_DUMP")) {
        FILE *d = fopen(getenv("AV1_ORACLE_DUMP"), "wb");
        if (d) { fwrite(stream, 1, stream_len, d); fclose(d);
                 printf("wrote the rebuilt stream to %s\n\n", getenv("AV1_ORACLE_DUMP")); }
    }

    if (decode_stream(original, original_size, &ref, "reference") < 0)
        return 1;
    if (decode_stream(stream, stream_len, &sub, "subject") < 0) {
        fprintf(stderr, "\nthe rebuilt stream does not decode at all. dav1d reports this as "
                "EINVAL and\n'Error parsing frame header', but that does not mean the header "
                "is malformed: a\nreference resolving to the wrong picture reads the same "
                "way. Check the slots first --\n./dpb-check.py resolve <original.obu> "
                "<rebuilt.obu> -- and only then the syntax.\n");
        return 1;
    }

    printf("reference decoded %u pictures, subject decoded %u\n", ref.n, sub.n);

    /* Match on frame_offset, not on position. The original clip shows a hidden frame later
     * with show_existing_frame, so it emits a picture for all sixty; the rebuilt stream
     * carries one decode per submission and emits a picture only for the frames the guest
     * marked shown. Comparing by index would pair different frames and report every one of
     * them as a pixel mismatch. */
    /* frame_offset is an order hint, a few bits wide, so it repeats every hundred-odd
     * frames; both decodes emit pictures in display order, so the counterpart is the next
     * reference picture with that offset after the previous match, never an earlier one. */
    unsigned r_next = 0;
    for (unsigned i = 0; i < sub.n; i++) {
        const char *why = NULL;
        unsigned r;

        for (r = r_next; r < ref.n; r++)
            if (ref.pics[r].frame_hdr->frame_offset == sub.pics[i].frame_hdr->frame_offset)
                break;
        if (r == ref.n) {
            printf("  subject picture %u (frame_offset %d) has no counterpart\n",
                   i, sub.pics[i].frame_hdr->frame_offset);
            mismatches++;
            continue;
        }
        r_next = r + 1;
        compared++;
        if (planes_equal(&ref.pics[r], &sub.pics[i], &why))
            continue;
        mismatches++;
        printf("\n  picture %u differs (%s)\n", i, why);
        if (mismatches <= 3)
            diff_headers(ref.pics[r].frame_hdr, sub.pics[i].frame_hdr);
    }

    if (getenv("AV1_ORACLE_HASH")) {
        /* Plane sums for cross-checking against vt-oracle on AV1 silicon. Printed for the
         * REBUILT stream's pictures, which is what the backend will actually submit.
         * VideoToolbox returns biplanar 4:2:0, so its chroma sum corresponds to plane 1 plus
         * plane 2 here; the combined value is printed to make that comparison direct. */
        printf("\nplane checksums (luma, chroma-combined):\n");
        for (unsigned i = 0; i < sub.n; i++) {
            const Dav1dPicture *p = &sub.pics[i];
            uint64_t s[3] = { 0, 0, 0 };

            for (int pl = 0; pl < 3; pl++) {
                int ss_ver = p->p.layout == DAV1D_PIXEL_LAYOUT_I420 ? 1 : 0;
                int ss_hor = p->p.layout != DAV1D_PIXEL_LAYOUT_I444 ? 1 : 0;
                int w = pl ? (p->p.w + ss_hor) >> ss_hor : p->p.w;
                int h = pl ? (p->p.h + ss_ver) >> ss_ver : p->p.h;
                const uint8_t *base = p->data[pl];

                if (!base)
                    continue;
                for (int y = 0; y < h; y++)
                    for (int x = 0; x < w; x++)
                        s[pl] += base[(ptrdiff_t)y * p->stride[pl > 0] + x];
            }
            if (getenv("AV1_ORACLE_DUMP")) {
                char path[512];
                FILE *f;

                snprintf(path, sizeof(path), "%s/oh%03d.pgm", getenv("AV1_ORACLE_DUMP"),
                         p->frame_hdr->frame_offset);
                f = fopen(path, "wb");
                if (f) {
                    fprintf(f, "P5\n%d %d\n255\n", p->p.w, p->p.h);
                    for (int y = 0; y < p->p.h; y++)
                        fwrite((const uint8_t *)p->data[0] + (ptrdiff_t)y * p->stride[0],
                               1, (size_t)p->p.w, f);
                    fclose(f);
                }
            }
            if (getenv("AV1_ORACLE_LEFT")) {
                /* Sum the leftmost N luma columns. VideoToolbox returns superres frames at
                 * the CODED width; if what it returns is a left crop of the upscaled picture
                 * rather than the un-upscaled one, this matches its checksum exactly. */
                int n = atoi(getenv("AV1_ORACLE_LEFT"));
                uint64_t l = 0;

                if (n > p->p.w)
                    n = p->p.w;
                for (int y = 0; y < p->p.h; y++)
                    for (int x = 0; x < n; x++)
                        l += ((const uint8_t *)p->data[0])[(ptrdiff_t)y * p->stride[0] + x];
                printf("  oh=%-3d left%-4d %016llx mean=%.4f\n",
                       p->frame_hdr->frame_offset, n, (unsigned long long)l,
                       (double)l / ((double)n * p->p.h));
                continue;
            }
            printf("  oh=%-3d %016llx %016llx %dx%d mean=%.4f\n",
                   p->frame_hdr->frame_offset,
                   (unsigned long long)s[0], (unsigned long long)(s[1] + s[2]),
                   p->p.w, p->p.h, (double)s[0] / ((double)p->p.w * p->p.h));
        }
    }

    for (unsigned i = 0; i < ref.n; i++) dav1d_picture_unref(&ref.pics[i]);
    for (unsigned i = 0; i < sub.n; i++) dav1d_picture_unref(&sub.pics[i]);
    free(original);
    free(stream);

    if (mismatches || !compared) {
        printf("\nFAIL: %u of %u compared pictures differ\n", mismatches, compared);
        return 1;
    }
    /* The frames the guest hid are not compared directly -- the rebuilt stream never shows
     * them, so no decoder emits them. They are still covered: every shown picture is
     * predicted from them, so a hidden frame decoded wrongly shows up as a pixel difference
     * in the frames that reference it. */
    printf("\nPASS: all %u shown pictures are bit-identical to the original stream's decode "
           "(%u hidden frames covered as references)\n", compared, frames - compared);
    return 0;
}
