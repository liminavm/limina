/* SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
 * Copyright © 2026 Gustavo Noronha Silva
 *
 * Report which AV1 frame-header syntax the captured fixtures actually exercise.
 *
 * The serializer is only tested on the syntax its fixtures reach, so "we encoded a clip
 * with film grain" is worth nothing until the descriptors show grain arriving. This reads
 * the raw `struct virgl_av1_picture_desc` dumps written by the capture build and counts
 * the branches that matter to a writer.
 *
 * Build: see Makefile. It includes virglrenderer's own header, so the struct layout is by
 * construction the one the capture wrote.
 */

#include <dirent.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "virgl_video_hw.h"

struct counts {
    unsigned frames;
    unsigned key, inter, intra_only, switch_frame;
    unsigned no_show, showable;
    unsigned primary_ref_none, primary_ref_set;
    unsigned seg_enabled, seg_update_data, seg_update_map, seg_temporal_update;
    unsigned lf_delta_enabled, lf_delta_update;
    unsigned grain_applied;
    unsigned gm_nonidentity, gm_translation, gm_rotzoom, gm_affine;
    unsigned superres, multi_tile, cdef_on, lr_on, delta_q, delta_lf;
    unsigned intrabc, high_precision_mv, allow_warped, reduced_tx;
    unsigned max_tiles, max_slices;
};

static void tally(const struct virgl_av1_picture_desc *d, struct counts *c)
{
    const typeof(d->picture_parameter) *p = &d->picture_parameter;

    c->frames++;

    switch (p->pic_info_fields.frame_type) {
    case 0: c->key++; break;
    case 1: c->inter++; break;
    case 2: c->intra_only++; break;
    default: c->switch_frame++; break;
    }

    if (!p->pic_info_fields.show_frame) c->no_show++;
    if (p->pic_info_fields.showable_frame) c->showable++;

    /* 7 is PRIMARY_REF_NONE: nothing is inherited, so every delta codes against defaults. */
    if (p->primary_ref_frame == 7) c->primary_ref_none++;
    else                           c->primary_ref_set++;

    if (p->seg_info.segment_info_fields.enabled) {
        c->seg_enabled++;
        if (p->seg_info.segment_info_fields.update_data)      c->seg_update_data++;
        if (p->seg_info.segment_info_fields.update_map)       c->seg_update_map++;
        if (p->seg_info.segment_info_fields.temporal_update)  c->seg_temporal_update++;
    }

    if (p->loop_filter_info_fields.mode_ref_delta_enabled) {
        c->lf_delta_enabled++;
        if (p->loop_filter_info_fields.mode_ref_delta_update) c->lf_delta_update++;
    }

    if (p->film_grain_info.film_grain_info_fields.apply_grain) c->grain_applied++;

    /* Global motion is the one thing a writer cannot re-emit from the descriptor alone:
     * it is coded as a subexp delta against the primary reference's saved warp. */
    for (int i = 0; i < 7; i++) {
        switch (p->wm[i].wmtype) {
        case 0: break;                                  /* IDENTITY */
        case 1: c->gm_translation++; c->gm_nonidentity++; break;
        case 2: c->gm_rotzoom++;     c->gm_nonidentity++; break;
        default: c->gm_affine++;     c->gm_nonidentity++; break;
        }
    }

    if (p->pic_info_fields.use_superres)            c->superres++;
    if (p->tile_cols > 1 || p->tile_rows > 1)       c->multi_tile++;
    if (p->cdef_bits || p->cdef_y_strengths[0])     c->cdef_on++;
    if (p->loop_restoration_fields.yframe_restoration_type) c->lr_on++;
    if (p->mode_control_fields.delta_q_present_flag)  c->delta_q++;
    if (p->mode_control_fields.delta_lf_present_flag) c->delta_lf++;
    if (p->pic_info_fields.allow_intrabc)           c->intrabc++;
    if (p->pic_info_fields.allow_high_precision_mv) c->high_precision_mv++;
    if (p->pic_info_fields.allow_warped_motion)     c->allow_warped++;
    if (p->mode_control_fields.reduced_tx_set_used) c->reduced_tx++;

    unsigned tiles = (unsigned)p->tile_cols * p->tile_rows;
    if (tiles > c->max_tiles) c->max_tiles = tiles;
    if (d->slice_parameter.slice_count > c->max_slices)
        c->max_slices = d->slice_parameter.slice_count;
}

static void row(const char *name, unsigned n, unsigned total)
{
    printf("  %-22s %5u %s\n", name, n,
           n ? "" : "   <- NOT EXERCISED by any fixture");
    (void)total;
}

int main(int argc, char **argv)
{
    struct counts c = {0};

    if (argc < 2) {
        fprintf(stderr, "usage: %s <capture-dir>...\n", argv[0]);
        return 2;
    }

    for (int a = 1; a < argc; a++) {
        DIR *dir = opendir(argv[a]);
        struct dirent *e;

        if (!dir) { perror(argv[a]); return 1; }

        while ((e = readdir(dir))) {
            struct virgl_av1_picture_desc desc;
            char path[2048];
            FILE *f;
            size_t n;

            if (!strstr(e->d_name, ".desc"))
                continue;
            snprintf(path, sizeof(path), "%s/%s", argv[a], e->d_name);
            if (!(f = fopen(path, "rb")))
                continue;
            n = fread(&desc, 1, sizeof(desc), f);
            fclose(f);
            if (n != sizeof(desc)) {
                fprintf(stderr, "%s: short read (%zu of %zu) -- built against a "
                        "different virgl_video_hw.h?\n", path, n, sizeof(desc));
                return 1;
            }
            tally(&desc, &c);
        }
        closedir(dir);
    }

    printf("AV1 fixture coverage over %u frames\n\n", c.frames);
    printf(" frame types\n");
    row("key", c.key, c.frames);
    row("inter", c.inter, c.frames);
    row("intra_only", c.intra_only, c.frames);
    row("switch", c.switch_frame, c.frames);
    row("no-show", c.no_show, c.frames);
    row("showable", c.showable, c.frames);
    printf("\n inheritance (what a writer would have to resolve)\n");
    row("primary_ref NONE", c.primary_ref_none, c.frames);
    row("primary_ref set", c.primary_ref_set, c.frames);
    printf("\n syntax a writer must emit\n");
    row("segmentation", c.seg_enabled, c.frames);
    row("  update_data", c.seg_update_data, c.frames);
    row("  update_map", c.seg_update_map, c.frames);
    row("  temporal_update", c.seg_temporal_update, c.frames);
    row("lf delta enabled", c.lf_delta_enabled, c.frames);
    row("  delta update", c.lf_delta_update, c.frames);
    row("film grain", c.grain_applied, c.frames);
    row("global motion", c.gm_nonidentity, c.frames);
    row("  translation", c.gm_translation, c.frames);
    row("  rotzoom", c.gm_rotzoom, c.frames);
    row("  affine", c.gm_affine, c.frames);
    row("superres", c.superres, c.frames);
    row("multi-tile", c.multi_tile, c.frames);
    row("cdef", c.cdef_on, c.frames);
    row("loop restoration", c.lr_on, c.frames);
    row("delta_q", c.delta_q, c.frames);
    row("delta_lf", c.delta_lf, c.frames);
    row("intrabc", c.intrabc, c.frames);
    row("high-precision mv", c.high_precision_mv, c.frames);
    row("warped motion", c.allow_warped, c.frames);
    row("reduced tx set", c.reduced_tx, c.frames);
    printf("\n maxima: %u tiles, %u slices per frame\n", c.max_tiles, c.max_slices);

    return 0;
}
