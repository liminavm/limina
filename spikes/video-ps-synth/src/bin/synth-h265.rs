//! Build the HEVC parameter sets the shipping Rust backend would synthesize for a stream, from
//! the values mesa puts on the wire, and write them as an Annex-B prefix.
//!
//! A port of `spikes/hevc-ps-synth/synth.c`, which graded the C serializer, onto `virglrs`'s
//! `PictureDesc::parameter_sets`.
//!
//! Deliberately NOT handed over, because they are what the backend has to invent:
//!   - the whole VPS
//!   - `general_level_idc` (over-declared to a constant, since a level change is the one
//!     format-description delta a live session refuses)
//!   - `conf_win_*`, derived from coded size vs display size
//!   - the `short_term_ref_pic_set` contents, which are absent from VA-API by design

use video_ps_synth::{args, write_annexb};
use virglrenderer::vrend::video::h265::{HevcProfile, PictureDesc, Tiles};

fn main() {
    let (f, w, h, out) = args("synth-h265");

    // The C harness refused a stream carrying custom scaling lists and filled the default ones
    // otherwise. The Rust descriptor says the same thing with one flag instead of an array.
    let scaling_list_enabled = f.flag("sps_scaling_list_enabled_flag", false);
    if scaling_list_enabled
        && (f.flag("sps_sps_scaling_list_data_present_flag", false)
            || f.flag("pps_pps_scaling_list_data_present_flag", false))
    {
        eprintln!(
            "  stream carries CUSTOM scaling lists; the harness models only the default ones"
        );
        std::process::exit(1);
    }

    // Uniform spacing writes only the counts and never reads the wire's size arrays; explicit
    // spacing writes one size per tile. The C harness left the size arrays zeroed, so explicit
    // spacing reproduces that as zero-filled vectors of the right length.
    let tiles = f.flag("pps_tiles_enabled_flag", false).then(|| {
        let columns_minus1 = f.u32("pps_num_tile_columns_minus1", 0);
        let rows_minus1 = f.u32("pps_num_tile_rows_minus1", 0);
        if f.flag("pps_uniform_spacing_flag", true) {
            Tiles::Uniform {
                columns_minus1,
                rows_minus1,
            }
        } else {
            Tiles::Explicit {
                column_widths_minus1: vec![0u16; columns_minus1 as usize + 1],
                row_heights_minus1: vec![0u16; rows_minus1 as usize + 1],
            }
        }
    });

    let desc = PictureDesc {
        // Unlike H.264, the coded geometry IS on the wire.
        pic_width_in_luma_samples: f.u32("sps_pic_width_in_luma_samples", w as i64),
        pic_height_in_luma_samples: f.u32("sps_pic_height_in_luma_samples", h as i64),

        chroma_format_idc: f.u8("sps_chroma_format_idc", 1),
        separate_colour_plane: f.flag("sps_separate_colour_plane_flag", false),
        bit_depth_luma_minus8: f.u8("sps_bit_depth_luma_minus8", 0),
        bit_depth_chroma_minus8: f.u8("sps_bit_depth_chroma_minus8", 0),
        log2_max_pic_order_cnt_lsb_minus4: f.u8("sps_log2_max_pic_order_cnt_lsb_minus4", 0),
        sps_max_dec_pic_buffering_minus1: f.u8("sps_sps_max_dec_pic_buffering_minus1", 1),
        log2_min_luma_coding_block_size_minus3: f
            .u8("sps_log2_min_luma_coding_block_size_minus3", 0),
        log2_diff_max_min_luma_coding_block_size: f
            .u8("sps_log2_diff_max_min_luma_coding_block_size", 0),
        log2_min_transform_block_size_minus2: f
            .u8("sps_log2_min_luma_transform_block_size_minus2", 0),
        log2_diff_max_min_transform_block_size: f
            .u8("sps_log2_diff_max_min_luma_transform_block_size", 0),
        max_transform_hierarchy_depth_inter: f.u8("sps_max_transform_hierarchy_depth_inter", 0),
        max_transform_hierarchy_depth_intra: f.u8("sps_max_transform_hierarchy_depth_intra", 0),
        scaling_lists_are_default: true,
        scaling_list_enabled,
        amp_enabled: f.flag("sps_amp_enabled_flag", false),
        sample_adaptive_offset_enabled: f.flag("sps_sample_adaptive_offset_enabled_flag", false),
        pcm_enabled: f.flag("sps_pcm_enabled_flag", false),
        pcm_sample_bit_depth_luma_minus1: f.u8("sps_pcm_sample_bit_depth_luma_minus1", 0),
        pcm_sample_bit_depth_chroma_minus1: f.u8("sps_pcm_sample_bit_depth_chroma_minus1", 0),
        log2_min_pcm_luma_coding_block_size_minus3: f
            .u8("sps_log2_min_pcm_luma_coding_block_size_minus3", 0),
        log2_diff_max_min_pcm_luma_coding_block_size: f
            .u8("sps_log2_diff_max_min_pcm_luma_coding_block_size", 0),
        pcm_loop_filter_disabled: f.flag("sps_pcm_loop_filter_disabled_flag", false),
        num_short_term_ref_pic_sets: f.u8("sps_num_short_term_ref_pic_sets", 0),
        long_term_ref_pics_present: f.flag("sps_long_term_ref_pics_present_flag", false),
        num_long_term_ref_pics_sps: f.u8("sps_num_long_term_ref_pics_sps", 0),
        sps_temporal_mvp_enabled: f.flag("sps_sps_temporal_mvp_enabled_flag", false),
        strong_intra_smoothing_enabled: f.flag("sps_strong_intra_smoothing_enabled_flag", false),

        dependent_slice_segments_enabled: f
            .flag("pps_dependent_slice_segments_enabled_flag", false),
        output_flag_present: f.flag("pps_output_flag_present_flag", false),
        num_extra_slice_header_bits: f.u8("pps_num_extra_slice_header_bits", 0),
        sign_data_hiding_enabled: f.flag("pps_sign_data_hiding_enabled_flag", false),
        cabac_init_present: f.flag("pps_cabac_init_present_flag", false),
        num_ref_idx_l0_default_active_minus1: f.u8("pps_num_ref_idx_l0_default_active_minus1", 0),
        num_ref_idx_l1_default_active_minus1: f.u8("pps_num_ref_idx_l1_default_active_minus1", 0),
        init_qp_minus26: f.i8("pps_init_qp_minus26", 0),
        constrained_intra_pred: f.flag("pps_constrained_intra_pred_flag", false),
        transform_skip_enabled: f.flag("pps_transform_skip_enabled_flag", false),
        cu_qp_delta_enabled: f.flag("pps_cu_qp_delta_enabled_flag", false),
        diff_cu_qp_delta_depth: f.u8("pps_diff_cu_qp_delta_depth", 0),
        pps_cb_qp_offset: f.i8("pps_pps_cb_qp_offset", 0),
        pps_cr_qp_offset: f.i8("pps_pps_cr_qp_offset", 0),
        pps_slice_chroma_qp_offsets_present: f
            .flag("pps_pps_slice_chroma_qp_offsets_present_flag", false),
        weighted_pred: f.flag("pps_weighted_pred_flag", false),
        weighted_bipred: f.flag("pps_weighted_bipred_flag", false),
        transquant_bypass_enabled: f.flag("pps_transquant_bypass_enabled_flag", false),
        entropy_coding_sync_enabled: f.flag("pps_entropy_coding_sync_enabled_flag", false),
        tiles,
        loop_filter_across_tiles_enabled: f.flag("pps_loop_filter_across_tiles_enabled_flag", true),
        pps_loop_filter_across_slices_enabled: f
            .flag("pps_pps_loop_filter_across_slices_enabled_flag", false),
        deblocking_filter_control_present: f
            .flag("pps_deblocking_filter_control_present_flag", false),
        deblocking_filter_override_enabled: f
            .flag("pps_deblocking_filter_override_enabled_flag", false),
        pps_deblocking_filter_disabled: f.flag("pps_pps_deblocking_filter_disabled_flag", false),
        pps_beta_offset_div2: f.i8("pps_pps_beta_offset_div2", 0),
        pps_tc_offset_div2: f.i8("pps_pps_tc_offset_div2", 0),
        lists_modification_present: f.flag("pps_lists_modification_present_flag", false),
        log2_parallel_merge_level_minus2: f.u8("pps_log2_parallel_merge_level_minus2", 0),
        slice_segment_header_extension_present: f
            .flag("pps_slice_segment_header_extension_present_flag", false),
        // Not part of any parameter set — it is what the frame gate asks, and nothing here asks it.
        key: false,
    };

    match desc.parameter_sets(w, h, HevcProfile::Main) {
        Ok(ps) => {
            eprintln!(
                "  synthesized: vps {}, sps {}, pps {} bytes",
                ps.vps.len(),
                ps.sps.len(),
                ps.pps.len()
            );
            write_annexb(&out, &[&ps.vps, &ps.sps, &ps.pps]);
        }
        Err(e) => {
            eprintln!("  parameter_sets refused this stream: {e:?}");
            std::process::exit(1);
        }
    }
}
