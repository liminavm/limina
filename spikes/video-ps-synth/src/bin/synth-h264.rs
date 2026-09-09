//! Build the H.264 parameter sets the shipping Rust backend would synthesize for a stream, from
//! the values mesa would put on the wire, and write them as an Annex-B prefix.
//!
//! This is a port of `spikes/h264-ps-synth/synth.c`, which graded the C serializer. It calls
//! `virglrs`'s `PictureDesc::parameter_sets` directly — the descriptor fields are public, so
//! nothing here reconstructs the wire layout, and there is no copy of the C to drift against.
//!
//! Note which keys are absent on purpose. Nothing feeds `pic_width_in_mbs_minus1`,
//! `pic_height_in_map_units_minus1` or any `frame_crop_*` value: those are what the serializer
//! has to DERIVE from the display size, so handing them over would test nothing.

use video_ps_synth::{args, write_annexb};
use virglrenderer::vrend::video::h264::{H264Profile, PictureDesc};

fn profile_of(idc: i64) -> H264Profile {
    match idc {
        66 => H264Profile::Baseline,
        77 => H264Profile::Main,
        100 => H264Profile::High,
        // Extended (88) is refused rather than folded into a neighbour. The serializer has three
        // profiles because Baseline and Constrained Baseline share a profile_idc and a constraint
        // flag only ever narrows one; Extended is a fourth thing, and mapping it to any of the
        // three writes a profile_idc the stream did not have. No clip here codes it, so a guess
        // would be an untested one — and refusing is what the serializer does with every other
        // stream it cannot describe.
        _ => {
            eprintln!("unhandled profile_idc {idc}");
            std::process::exit(1);
        }
    }
}

fn main() {
    let (f, w, h, out) = args("synth-h264");

    // `offset_for_ref_frame`'s length IS num_ref_frames_in_pic_order_cnt_cycle in the Rust
    // descriptor, so the count and the array cannot be handed on disagreeing. The C harness
    // set the count and left the array zeroed; this reproduces that exactly.
    let cycle = f.get("sps_num_ref_frames_in_pic_order_cnt_cycle", 0).max(0) as usize;

    let desc = PictureDesc {
        level_idc: f.u8("sps_level_idc", 30),
        chroma_format_idc: f.u8("sps_chroma_format_idc", 1),
        separate_colour_plane: f.flag("sps_separate_colour_plane_flag", false),
        bit_depth_luma_minus8: f.u8("sps_bit_depth_luma_minus8", 0),
        bit_depth_chroma_minus8: f.u8("sps_bit_depth_chroma_minus8", 0),
        log2_max_frame_num_minus4: f.u8("sps_log2_max_frame_num_minus4", 0),
        pic_order_cnt_type: f.u8("sps_pic_order_cnt_type", 0),
        log2_max_pic_order_cnt_lsb_minus4: f.u8("sps_log2_max_pic_order_cnt_lsb_minus4", 0),
        delta_pic_order_always_zero: f.flag("sps_delta_pic_order_always_zero_flag", false),
        offset_for_non_ref_pic: f.get("sps_offset_for_non_ref_pic", 0) as i32,
        offset_for_top_to_bottom_field: f.get("sps_offset_for_top_to_bottom_field", 0) as i32,
        offset_for_ref_frame: vec![0i32; cycle],
        frame_mbs_only: f.flag("sps_frame_mbs_only_flag", true),
        direct_8x8_inference: f.flag("sps_direct_8x8_inference_flag", true),

        entropy_coding_mode: f.flag("pps_entropy_coding_mode_flag", false),
        bottom_field_pic_order_in_frame_present: f
            .flag("pps_bottom_field_pic_order_in_frame_present_flag", false),
        num_slice_groups_minus1: f.u8("pps_num_slice_groups_minus1", 0),
        weighted_pred: f.flag("pps_weighted_pred_flag", false),
        weighted_bipred_idc: f.u8("pps_weighted_bipred_idc", 0),
        pic_init_qp_minus26: f.i8("pps_pic_init_qp_minus26", 0),
        pic_init_qs_minus26: f.i8("pps_pic_init_qs_minus26", 0),
        chroma_qp_index_offset: f.i8("pps_chroma_qp_index_offset", 0),
        deblocking_filter_control_present: f
            .flag("pps_deblocking_filter_control_present_flag", false),
        constrained_intra_pred: f.flag("pps_constrained_intra_pred_flag", false),
        redundant_pic_cnt_present: f.flag("pps_redundant_pic_cnt_present_flag", false),
        transform_8x8_mode: f.flag("pps_transform_8x8_mode_flag", false),
        second_chroma_qp_index_offset: f.i8("pps_second_chroma_qp_index_offset", 0),
        // The C harness memset the descriptor, so the wire's scaling arrays were all-zero,
        // which means "the guest sent no IQMatrix at all" and needs no signalling.
        scaling_matrix: false,

        field_pic: false,
        // These three live at the TOP LEVEL of the picture descriptor, not in the SPS/PPS, and
        // that distinction is the whole point. mesa's decode frontend never writes
        // sps.max_num_ref_frames or pps.num_ref_idx_l*_default_active_minus1 — only the encoder
        // path does — so a serializer reading those gets zeros and produces a stream
        // VideoToolbox rejects with kVTVideoDecoderBadDataErr from the third frame on. The first
        // version of this harness fed the SPS fields and therefore passed while the real guest
        // path was broken. Model the wire, not the spec's layout.
        num_ref_frames: f.u8("sps_max_num_ref_frames", 1),
        num_ref_idx_l0_active_minus1: f.u8("pps_num_ref_idx_l0_default_active_minus1", 0),
        num_ref_idx_l1_active_minus1: f.u8("pps_num_ref_idx_l1_default_active_minus1", 0),
    };

    let profile = profile_of(f.get("sps_profile_idc", 100));
    let pps_id = f.u32("pps_pic_parameter_set_id", 0);

    match desc.parameter_sets(w, h, profile, pps_id) {
        Ok(ps) => {
            eprintln!(
                "  synthesized: sps {} bytes, pps {} bytes",
                ps.sps.len(),
                ps.pps.len()
            );
            write_annexb(&out, &[&ps.sps, &ps.pps]);
        }
        Err(e) => {
            // A refusal is a result, not a crash: the serializer declining a stream it cannot
            // describe is the designed behaviour, and naming which refusal is the useful part.
            eprintln!("  parameter_sets refused this stream: {e:?}");
            std::process::exit(1);
        }
    }
}
