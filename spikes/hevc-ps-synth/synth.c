/*
 * Build the HEVC parameter sets our backend would synthesize for a given stream, from the
 * values mesa puts on the wire, and write them as an Annex-B prefix.
 *
 * As with the H.264 spike, the field values come from a key=value file that verify.sh
 * derives from the stream's OWN parameter sets, so the serializer never sees the original
 * bytes -- only the parsed semantics, which is the position the backend is in.
 *
 * Deliberately NOT handed over, because they are what the backend has to invent:
 *   - the whole VPS
 *   - general_level_idc (over-declared to a constant, since a level change is the one
 *     format-description delta a live session refuses)
 *   - conf_win_*, derived from coded size vs display size
 *   - the short_term_ref_pic_set contents, which are absent from VA-API by design
 */
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "virgl_video_h265_ps.h"

#define MAX_KEYS 256

static struct { char k[80]; long v; } keys[MAX_KEYS];
static unsigned nkeys;

static void load(const char *path)
{
    char line[256];
    FILE *f = fopen(path, "r");
    if (!f) { perror(path); exit(1); }
    while (fgets(line, sizeof(line), f)) {
        char *eq = strchr(line, '=');
        if (!eq) continue;
        if (nkeys >= MAX_KEYS) { fprintf(stderr, "too many keys\n"); exit(1); }
        *eq = 0;
        snprintf(keys[nkeys].k, sizeof(keys[nkeys].k), "%s", line);
        keys[nkeys].v = strtol(eq + 1, NULL, 10);
        nkeys++;
    }
    fclose(f);
}

static long get(const char *k, long dflt)
{
    for (unsigned i = 0; i < nkeys; i++)
        if (!strcmp(keys[i].k, k))
            return keys[i].v;
    return dflt;
}

/*
 * When a stream enables scaling lists without carrying data, mesa hands the backend the
 * DEFAULT lists -- it delivers the effective lists either way. The harness has to do the
 * same, or the serializer sees zeroes and refuses a stream it would accept in the guest.
 * (Custom lists are not modelled here; the serializer refuses those, and verifying a
 * refusal needs a stream that carries them.)
 */
static const uint8_t def_intra[64] = {
   16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 17, 16, 17, 16, 17, 18,
   17, 18, 18, 17, 18, 21, 19, 20, 21, 20, 19, 21, 24, 22, 22, 24,
   24, 22, 22, 24, 25, 25, 27, 30, 27, 25, 25, 29, 31, 35, 35, 31,
   29, 36, 41, 44, 41, 36, 47, 54, 54, 47, 65, 70, 65, 88, 88, 115,
};
static const uint8_t def_inter[64] = {
   16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 17, 17, 17, 17, 17, 18,
   18, 18, 18, 18, 18, 20, 20, 20, 20, 20, 20, 20, 24, 24, 24, 24,
   24, 24, 24, 24, 25, 25, 25, 25, 25, 25, 25, 28, 28, 28, 28, 28,
   28, 33, 33, 33, 33, 33, 41, 41, 41, 41, 54, 54, 54, 71, 71, 91,
};

static void fill_default_scaling_lists(struct virgl_h265_sps *s)
{
    for (unsigned m = 0; m < 6; m++) {
        const uint8_t *d = m < 3 ? def_intra : def_inter;
        memset(s->ScalingList4x4[m], 16, 16);
        memcpy(s->ScalingList8x8[m], d, 64);
        memcpy(s->ScalingList16x16[m], d, 64);
        s->ScalingListDCCoeff16x16[m] = 16;
    }
    for (unsigned m = 0; m < 2; m++) {
        memcpy(s->ScalingList32x32[m], m ? def_inter : def_intra, 64);
        s->ScalingListDCCoeff32x32[m] = 16;
    }
}

int main(int argc, char **argv)
{
    struct virgl_h265_picture_desc d;
    struct virgl_h265_parameter_sets ps;
    static const uint8_t sc[4] = { 0, 0, 0, 1 };
    FILE *f;
    unsigned w, h;

    if (argc < 5) {
        fprintf(stderr, "usage: synth <fields> <width> <height> <out.bin>\n");
        return 2;
    }
    load(argv[1]);
    w = (unsigned)strtoul(argv[2], NULL, 10);
    h = (unsigned)strtoul(argv[3], NULL, 10);

    memset(&d, 0, sizeof(d));
    struct virgl_h265_sps *s = &d.pps.sps;

    /* Unlike H.264, the coded geometry IS on the wire. */
    s->pic_width_in_luma_samples  = (uint32_t)get("sps_pic_width_in_luma_samples", w);
    s->pic_height_in_luma_samples = (uint32_t)get("sps_pic_height_in_luma_samples", h);

    s->chroma_format_idc               = get("sps_chroma_format_idc", 1);
    s->separate_colour_plane_flag      = get("sps_separate_colour_plane_flag", 0);
    s->bit_depth_luma_minus8           = get("sps_bit_depth_luma_minus8", 0);
    s->bit_depth_chroma_minus8         = get("sps_bit_depth_chroma_minus8", 0);
    s->log2_max_pic_order_cnt_lsb_minus4 = get("sps_log2_max_pic_order_cnt_lsb_minus4", 0);
    s->sps_max_dec_pic_buffering_minus1  = get("sps_sps_max_dec_pic_buffering_minus1", 1);
    s->log2_min_luma_coding_block_size_minus3 =
        get("sps_log2_min_luma_coding_block_size_minus3", 0);
    s->log2_diff_max_min_luma_coding_block_size =
        get("sps_log2_diff_max_min_luma_coding_block_size", 0);
    s->log2_min_transform_block_size_minus2 =
        get("sps_log2_min_luma_transform_block_size_minus2", 0);
    s->log2_diff_max_min_transform_block_size =
        get("sps_log2_diff_max_min_luma_transform_block_size", 0);
    s->max_transform_hierarchy_depth_inter = get("sps_max_transform_hierarchy_depth_inter", 0);
    s->max_transform_hierarchy_depth_intra = get("sps_max_transform_hierarchy_depth_intra", 0);
    s->scaling_list_enabled_flag       = get("sps_scaling_list_enabled_flag", 0);
    s->amp_enabled_flag                = get("sps_amp_enabled_flag", 0);
    s->sample_adaptive_offset_enabled_flag = get("sps_sample_adaptive_offset_enabled_flag", 0);
    s->pcm_enabled_flag                = get("sps_pcm_enabled_flag", 0);
    s->pcm_sample_bit_depth_luma_minus1 = get("sps_pcm_sample_bit_depth_luma_minus1", 0);
    s->pcm_sample_bit_depth_chroma_minus1 = get("sps_pcm_sample_bit_depth_chroma_minus1", 0);
    s->log2_min_pcm_luma_coding_block_size_minus3 =
        get("sps_log2_min_pcm_luma_coding_block_size_minus3", 0);
    s->log2_diff_max_min_pcm_luma_coding_block_size =
        get("sps_log2_diff_max_min_pcm_luma_coding_block_size", 0);
    s->pcm_loop_filter_disabled_flag   = get("sps_pcm_loop_filter_disabled_flag", 0);
    s->num_short_term_ref_pic_sets     = get("sps_num_short_term_ref_pic_sets", 0);
    s->long_term_ref_pics_present_flag = get("sps_long_term_ref_pics_present_flag", 0);
    s->num_long_term_ref_pics_sps      = get("sps_num_long_term_ref_pics_sps", 0);
    s->sps_temporal_mvp_enabled_flag   = get("sps_sps_temporal_mvp_enabled_flag", 0);
    s->strong_intra_smoothing_enabled_flag =
        get("sps_strong_intra_smoothing_enabled_flag", 0);

    struct virgl_h265_pps *p = &d.pps;
    p->dependent_slice_segments_enabled_flag =
        get("pps_dependent_slice_segments_enabled_flag", 0);
    p->output_flag_present_flag        = get("pps_output_flag_present_flag", 0);
    p->num_extra_slice_header_bits     = get("pps_num_extra_slice_header_bits", 0);
    p->sign_data_hiding_enabled_flag   = get("pps_sign_data_hiding_enabled_flag", 0);
    p->cabac_init_present_flag         = get("pps_cabac_init_present_flag", 0);
    p->num_ref_idx_l0_default_active_minus1 =
        get("pps_num_ref_idx_l0_default_active_minus1", 0);
    p->num_ref_idx_l1_default_active_minus1 =
        get("pps_num_ref_idx_l1_default_active_minus1", 0);
    p->init_qp_minus26                 = get("pps_init_qp_minus26", 0);
    p->constrained_intra_pred_flag     = get("pps_constrained_intra_pred_flag", 0);
    p->transform_skip_enabled_flag     = get("pps_transform_skip_enabled_flag", 0);
    p->cu_qp_delta_enabled_flag        = get("pps_cu_qp_delta_enabled_flag", 0);
    p->diff_cu_qp_delta_depth          = get("pps_diff_cu_qp_delta_depth", 0);
    p->pps_cb_qp_offset                = get("pps_pps_cb_qp_offset", 0);
    p->pps_cr_qp_offset                = get("pps_pps_cr_qp_offset", 0);
    p->pps_slice_chroma_qp_offsets_present_flag =
        get("pps_pps_slice_chroma_qp_offsets_present_flag", 0);
    p->weighted_pred_flag              = get("pps_weighted_pred_flag", 0);
    p->weighted_bipred_flag            = get("pps_weighted_bipred_flag", 0);
    p->transquant_bypass_enabled_flag  = get("pps_transquant_bypass_enabled_flag", 0);
    p->tiles_enabled_flag              = get("pps_tiles_enabled_flag", 0);
    p->entropy_coding_sync_enabled_flag = get("pps_entropy_coding_sync_enabled_flag", 0);
    p->num_tile_columns_minus1         = get("pps_num_tile_columns_minus1", 0);
    p->num_tile_rows_minus1            = get("pps_num_tile_rows_minus1", 0);
    p->uniform_spacing_flag            = get("pps_uniform_spacing_flag", 1);
    p->loop_filter_across_tiles_enabled_flag =
        get("pps_loop_filter_across_tiles_enabled_flag", 1);
    p->pps_loop_filter_across_slices_enabled_flag =
        get("pps_pps_loop_filter_across_slices_enabled_flag", 0);
    p->deblocking_filter_control_present_flag =
        get("pps_deblocking_filter_control_present_flag", 0);
    p->deblocking_filter_override_enabled_flag =
        get("pps_deblocking_filter_override_enabled_flag", 0);
    p->pps_deblocking_filter_disabled_flag =
        get("pps_pps_deblocking_filter_disabled_flag", 0);
    p->pps_beta_offset_div2            = get("pps_pps_beta_offset_div2", 0);
    p->pps_tc_offset_div2              = get("pps_pps_tc_offset_div2", 0);
    p->lists_modification_present_flag = get("pps_lists_modification_present_flag", 0);
    p->log2_parallel_merge_level_minus2 = get("pps_log2_parallel_merge_level_minus2", 0);
    p->slice_segment_header_extension_present_flag =
        get("pps_slice_segment_header_extension_present_flag", 0);

    if (s->scaling_list_enabled_flag) {
        if (get("sps_sps_scaling_list_data_present_flag", 0) ||
            get("pps_pps_scaling_list_data_present_flag", 0)) {
            fprintf(stderr, "  stream carries CUSTOM scaling lists; the harness models only"
                            " the default ones\n");
            return 1;
        }
        fill_default_scaling_lists(s);
    }

    if (virgl_h265_build_parameter_sets(&d, w, h, PIPE_VIDEO_PROFILE_HEVC_MAIN, &ps)) {
        fprintf(stderr, "build_parameter_sets failed\n");
        return 1;
    }
    fprintf(stderr, "  synthesized: vps %zu, sps %zu, pps %zu bytes\n",
            ps.vps_len, ps.sps_len, ps.pps_len);

    f = fopen(argv[4], "wb");
    if (!f) { perror(argv[4]); return 1; }
    fwrite(sc, 1, 4, f); fwrite(ps.vps, 1, ps.vps_len, f);
    fwrite(sc, 1, 4, f); fwrite(ps.sps, 1, ps.sps_len, f);
    fwrite(sc, 1, 4, f); fwrite(ps.pps, 1, ps.pps_len, f);
    fclose(f);
    return 0;
}
