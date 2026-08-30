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
    unsigned frames = 0, mismatches = 0;
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

        if (stream_len + (1 << 20) > stream_cap) {
            stream_cap *= 2;
            stream = realloc(stream, stream_cap);
            if (!stream) return 1;
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

    if (!frames) {
        fprintf(stderr, "no fixtures in %s\n", capture_dir);
        return 1;
    }

    printf("rebuilt %u frames into %zu bytes (original clip: %zu)\n\n",
           frames, stream_len, original_size);

    if (decode_stream(original, original_size, &ref, "reference") < 0)
        return 1;
    if (decode_stream(stream, stream_len, &sub, "subject") < 0) {
        fprintf(stderr, "\nthe rebuilt stream does not decode at all -- so the frame header "
                "is malformed, not merely wrong.\n");
        return 1;
    }

    printf("reference decoded %u pictures, subject decoded %u\n", ref.n, sub.n);
    if (ref.n != sub.n)
        printf("  ^ a count mismatch is itself a failure: a no-show frame that turns into a "
               "shown one, or vice versa, changes it.\n");

    for (unsigned i = 0; i < ref.n && i < sub.n; i++) {
        const char *why = NULL;

        if (planes_equal(&ref.pics[i], &sub.pics[i], &why))
            continue;
        mismatches++;
        printf("\n  picture %u differs (%s)\n", i, why);
        if (mismatches <= 3)
            diff_headers(ref.pics[i].frame_hdr, sub.pics[i].frame_hdr);
    }

    for (unsigned i = 0; i < ref.n; i++) dav1d_picture_unref(&ref.pics[i]);
    for (unsigned i = 0; i < sub.n; i++) dav1d_picture_unref(&sub.pics[i]);
    free(original);
    free(stream);

    if (mismatches || ref.n != sub.n) {
        printf("\nFAIL: %u of %u pictures differ\n", mismatches, ref.n);
        return 1;
    }
    printf("\nPASS: every picture is bit-identical to the original stream's decode\n");
    return 0;
}
