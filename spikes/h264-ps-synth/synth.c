/*
 * Build the parameter sets our backend would synthesize for a given stream, from the
 * values mesa would put on the wire, and write them as an Annex-B prefix.
 *
 * The field values come from a key=value file that verify.sh derives from the stream's
 * OWN parameter sets with `ffmpeg -bsf:v trace_headers`. That is deliberate: the
 * serializer never sees the original bytes, only the parsed semantics the guest sends,
 * which is exactly the position the backend is in.
 *
 * Note which keys are absent on purpose. Nothing feeds pic_width_in_mbs_minus1,
 * pic_height_in_map_units_minus1 or any frame_crop_* value: those are what the serializer
 * has to DERIVE from the display size, so handing them over would test nothing.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "virgl_video_h264_ps.h"

#define MAX_KEYS 256

static struct { char k[64]; long v; } keys[MAX_KEYS];
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

static enum pipe_video_profile profile_of(long idc)
{
    switch (idc) {
    case 66:  return PIPE_VIDEO_PROFILE_MPEG4_AVC_BASELINE;
    case 77:  return PIPE_VIDEO_PROFILE_MPEG4_AVC_MAIN;
    case 88:  return PIPE_VIDEO_PROFILE_MPEG4_AVC_EXTENDED;
    case 100: return PIPE_VIDEO_PROFILE_MPEG4_AVC_HIGH;
    default:
        fprintf(stderr, "unhandled profile_idc %ld\n", idc);
        exit(1);
    }
}

int main(int argc, char **argv)
{
    struct virgl_h264_picture_desc d;
    struct virgl_h264_parameter_sets ps;
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
    d.pps.sps.level_idc                        = get("sps_level_idc", 30);
    d.pps.sps.chroma_format_idc                = get("sps_chroma_format_idc", 1);
    d.pps.sps.separate_colour_plane_flag        = get("sps_separate_colour_plane_flag", 0);
    d.pps.sps.bit_depth_luma_minus8            = get("sps_bit_depth_luma_minus8", 0);
    d.pps.sps.bit_depth_chroma_minus8          = get("sps_bit_depth_chroma_minus8", 0);
    d.pps.sps.log2_max_frame_num_minus4        = get("sps_log2_max_frame_num_minus4", 0);
    d.pps.sps.pic_order_cnt_type               = get("sps_pic_order_cnt_type", 0);
    d.pps.sps.log2_max_pic_order_cnt_lsb_minus4 = get("sps_log2_max_pic_order_cnt_lsb_minus4", 0);
    d.pps.sps.delta_pic_order_always_zero_flag = get("sps_delta_pic_order_always_zero_flag", 0);
    d.pps.sps.offset_for_non_ref_pic           = get("sps_offset_for_non_ref_pic", 0);
    d.pps.sps.offset_for_top_to_bottom_field   = get("sps_offset_for_top_to_bottom_field", 0);
    d.pps.sps.num_ref_frames_in_pic_order_cnt_cycle =
        get("sps_num_ref_frames_in_pic_order_cnt_cycle", 0);
    d.pps.sps.direct_8x8_inference_flag        = get("sps_direct_8x8_inference_flag", 1);
    d.pps.sps.frame_mbs_only_flag              = get("sps_frame_mbs_only_flag", 1);

    /*
     * These three live at the TOP LEVEL of the picture descriptor, not in the SPS/PPS, and
     * that distinction is the whole point. mesa's decode frontend never writes
     * sps.max_num_ref_frames or pps.num_ref_idx_l*_default_active_minus1 -- only the
     * encoder path does -- so a serializer reading those gets zeros and produces a stream
     * VideoToolbox rejects with kVTVideoDecoderBadDataErr from the third frame on. The
     * first version of this harness fed the SPS fields and therefore passed while the real
     * guest path was broken. Model the wire, not the spec's layout.
     */
    d.num_ref_frames                = get("sps_max_num_ref_frames", 1);
    d.num_ref_idx_l0_active_minus1  = get("pps_num_ref_idx_l0_default_active_minus1", 0);
    d.num_ref_idx_l1_active_minus1  = get("pps_num_ref_idx_l1_default_active_minus1", 0);

    d.pps.entropy_coding_mode_flag             = get("pps_entropy_coding_mode_flag", 0);
    d.pps.bottom_field_pic_order_in_frame_present_flag =
        get("pps_bottom_field_pic_order_in_frame_present_flag", 0);
    d.pps.num_slice_groups_minus1              = get("pps_num_slice_groups_minus1", 0);
    d.pps.weighted_pred_flag                   = get("pps_weighted_pred_flag", 0);
    d.pps.weighted_bipred_idc                  = get("pps_weighted_bipred_idc", 0);
    d.pps.pic_init_qp_minus26                  = get("pps_pic_init_qp_minus26", 0);
    d.pps.pic_init_qs_minus26                  = get("pps_pic_init_qs_minus26", 0);
    d.pps.chroma_qp_index_offset               = get("pps_chroma_qp_index_offset", 0);
    d.pps.deblocking_filter_control_present_flag =
        get("pps_deblocking_filter_control_present_flag", 0);
    d.pps.constrained_intra_pred_flag          = get("pps_constrained_intra_pred_flag", 0);
    d.pps.redundant_pic_cnt_present_flag       = get("pps_redundant_pic_cnt_present_flag", 0);
    d.pps.transform_8x8_mode_flag              = get("pps_transform_8x8_mode_flag", 0);
    d.pps.second_chroma_qp_index_offset        = get("pps_second_chroma_qp_index_offset", 0);

    if (virgl_h264_build_parameter_sets(&d, w, h,
                                        profile_of(get("sps_profile_idc", 100)),
                                        (unsigned)get("pps_pic_parameter_set_id", 0), &ps)) {
        fprintf(stderr, "build_parameter_sets failed\n");
        return 1;
    }
    fprintf(stderr, "  synthesized: sps %zu bytes, pps %zu bytes\n", ps.sps_len, ps.pps_len);

    f = fopen(argv[4], "wb");
    if (!f) { perror(argv[4]); return 1; }
    fwrite(sc, 1, 4, f); fwrite(ps.sps, 1, ps.sps_len, f);
    fwrite(sc, 1, 4, f); fwrite(ps.pps, 1, ps.pps_len, f);
    fclose(f);
    return 0;
}
