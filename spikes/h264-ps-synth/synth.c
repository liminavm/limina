/* Build the parameter sets our backend would synthesize for spikes/h264-ps-synth/ref.264,
 * from the values mesa would have put on the wire, and write them as an Annex-B prefix.
 * The oracle is ffmpeg: prefix + the reference stream's own slices must decode
 * bit-identically to the reference stream. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "virgl_video_h264_ps.h"

int main(int argc, char **argv)
{
    struct virgl_h264_picture_desc d;
    struct virgl_h264_parameter_sets ps;
    static const uint8_t sc[4] = { 0, 0, 0, 1 };
    FILE *f;

    if (argc < 2) { fprintf(stderr, "usage: synth <out.bin>\n"); return 2; }

    memset(&d, 0, sizeof(d));
    /* SPS, as traced from ref.264 */
    d.pps.sps.level_idc = 30;
    d.pps.sps.chroma_format_idc = 1;
    d.pps.sps.bit_depth_luma_minus8 = 0;
    d.pps.sps.bit_depth_chroma_minus8 = 0;
    d.pps.sps.log2_max_frame_num_minus4 = 0;
    d.pps.sps.pic_order_cnt_type = 0;
    d.pps.sps.log2_max_pic_order_cnt_lsb_minus4 = 2;
    d.pps.sps.max_num_ref_frames = 4;
    d.pps.sps.direct_8x8_inference_flag = 1;
    /* PPS, as traced from ref.264 */
    d.pps.entropy_coding_mode_flag = 1;
    d.pps.bottom_field_pic_order_in_frame_present_flag = 0;
    d.pps.num_slice_groups_minus1 = 0;
    d.pps.num_ref_idx_l0_default_active_minus1 = 2;
    d.pps.num_ref_idx_l1_default_active_minus1 = 0;
    d.pps.weighted_pred_flag = 1;
    d.pps.weighted_bipred_idc = 2;
    d.pps.pic_init_qp_minus26 = -3;
    d.pps.pic_init_qs_minus26 = 0;
    d.pps.chroma_qp_index_offset = -2;
    d.pps.deblocking_filter_control_present_flag = 1;
    d.pps.constrained_intra_pred_flag = 0;
    d.pps.redundant_pic_cnt_present_flag = 0;
    d.pps.transform_8x8_mode_flag = 1;
    d.pps.second_chroma_qp_index_offset = -2;

    if (virgl_h264_build_parameter_sets(&d, 640, 480,
                                        PIPE_VIDEO_PROFILE_MPEG4_AVC_HIGH, 0, &ps)) {
        fprintf(stderr, "build_parameter_sets failed\n");
        return 1;
    }
    fprintf(stderr, "sps %zu bytes, pps %zu bytes\n", ps.sps_len, ps.pps_len);

    f = fopen(argv[1], "wb");
    if (!f) { perror("fopen"); return 1; }
    fwrite(sc, 1, 4, f); fwrite(ps.sps, 1, ps.sps_len, f);
    fwrite(sc, 1, 4, f); fwrite(ps.pps, 1, ps.pps_len, f);
    fclose(f);
    return 0;
}
