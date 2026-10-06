# runs/r3

| workload | windows | KK msg/s | KK wire KB/s | B/msg | KK sync/s | KK bridge thread-s/s | venus cmd/s | venus ring+ctx KB/s | venus thread-s/s | vrend cmd/s | vrend KB/s | msg per venus+vrend cmd | KK wire / stream bytes | KK query round trips/s | KK folded msg/s | folded msg per cmd |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| idle | 0 | 0 | 0.0 | 0.0 | 0.0 | 0.000 | 0 | 0.0 | 0.000 | 0 | 0.0 | 0.00 | 0.00 | 0.0 | 0 | 0.00 |
| vkmark | 6 | 284,791 | 5,157.3 | 18.1 | 3,193.8 | 0.421 | 22,412 | 1,205.8 | 0.215 | 88 | 4.9 | 12.66 | 4.26 | 12.8 | 123,685 | 5.50 |
| vk-replay | 5 | 230,986 | 19,844.5 | 85.9 | 6,681.6 | 0.790 | 67,535 | 4,923.6 | 0.322 | 322 | 19.0 | 3.40 | 4.01 | 782.2 | 125,747 | 1.85 |
| drawstorm | 1 | 1,780,997 | 49,535.5 | 27.8 | 870.0 | 0.413 | 1,724,932 | 79,301.2 | 0.520 | 0 | 0.0 | 1.03 | 0.62 | 9.0 | 1,742,354 | 1.01 |
| gl-replay-venus | 17 | 7,082 | 166.7 | 23.5 | 111.1 | 0.059 | 2,914 | 165.7 | 0.053 | 37 | 1.8 | 2.40 | 1.00 | 6.8 | 3,674 | 1.25 |
| gl-replay-vrend | 17 | 5,596 | 98.8 | 17.7 | 107.4 | 0.039 | 0 | 0.0 | 0.000 | 462 | 236.3 | 12.11 | 0.42 | 2.5 | 2,296 | 4.97 |
| glmark2-venus | 6 | 653,752 | 11,607.0 | 17.8 | 8,636.7 | 0.893 | 242,986 | 14,778.5 | 0.452 | 62 | 2.8 | 2.69 | 0.79 | 3,925.2 | 287,897 | 1.18 |
| glmark2-vrend | 6 | 802,138 | 14,223.9 | 17.7 | 17,320.0 | 1.055 | 0 | 0.0 | 0.000 | 68,556 | 23,615.9 | 11.70 | 0.60 | 6,938.2 | 340,065 | 4.96 |

## vkmark (6 windows)

| class | calls/s | wire KB/s | thread-s/s |
|---|---|---|---|
| async | 272,018 | 4,911.3 | 0.109 |
| local | 36,934 | 570.4 | 0.003 |
| create | 9,579 | 156.6 | 0.043 |
| diag | 6,404 | 77.0 | 0.003 |
| sync | 3,194 | 89.4 | 0.269 |

| function (top 14 by calls; r1-r2 logged only top lists, so their rows undercount) | class | calls/s | wire KB/s | thread-s/s |
|---|---|---|---|---|
| mtl_render_pass_attachment_descriptor_set_load_action | async | 36,929 | 590.9 | 0.001 |
| mtl_render_pass_descriptor_get_color_attachment | local | 31,797 | 508.8 | 0.003 |
| mtl_render_pass_attachment_descriptor_set_store_action | async | 31,184 | 498.9 | 0.000 |
| mtl_render_pass_attachment_descriptor_set_texture | async | 31,184 | 498.9 | 0.001 |
| mtl_render_pass_attachment_descriptor_set_resolve_texture | async | 25,438 | 407.0 | 0.001 |
| mtl_residency_set_request_residency | async | 12,714 | 152.6 | 0.019 |
| mtl_residency_set_commit | async | 12,714 | 152.6 | 0.001 |
| mtl_release | async | 9,577 | 114.9 | 0.003 |
| mtl_commit_options_add_feedback_handler | async | 6,359 | 178.0 | 0.002 |
| mtl_signal_event | async | 6,358 | 152.6 | 0.018 |
| mtl_render_pass_attachment_descriptor_set_level | async | 5,746 | 91.9 | 0.000 |
| mtl_render_pass_attachment_descriptor_set_slice | async | 5,746 | 91.9 | 0.000 |
| mtl_render_pass_descriptor_get_depth_attachment | local | 5,131 | 61.6 | 0.000 |
| mtl_set_address | async | 3,210 | 77.0 | 0.000 |

## vk-replay (5 windows)

| class | calls/s | wire KB/s | thread-s/s |
|---|---|---|---|
| async | 213,181 | 4,046.9 | 0.128 |
| local | 23,490 | 359.5 | 0.002 |
| diag | 15,627 | 211.2 | 0.003 |
| create | 11,123 | 15,612.2 | 0.072 |
| sync | 6,682 | 185.4 | 0.590 |

| function (top 14 by calls; r1-r2 logged only top lists, so their rows undercount) | class | calls/s | wire KB/s | thread-s/s |
|---|---|---|---|---|
| mtl_render_pass_attachment_descriptor_set_load_action | async | 23,293 | 372.7 | 0.000 |
| mtl_render_pass_descriptor_get_color_attachment | local | 19,412 | 310.6 | 0.002 |
| mtl_render_pass_attachment_descriptor_set_store_action | async | 19,411 | 310.6 | 0.000 |
| mtl_render_pass_attachment_descriptor_set_texture | async | 19,411 | 310.6 | 0.001 |
| mtl_render_pass_attachment_descriptor_set_resolve_texture | async | 15,529 | 248.5 | 0.000 |
| mtl_residency_set_commit | async | 11,765 | 141.2 | 0.004 |
| mtl_residency_set_request_residency | async | 11,765 | 141.2 | 0.018 |
| mtl_release | async | 11,164 | 134.0 | 0.010 |
| mtl_signal_event | async | 9,815 | 235.6 | 0.021 |
| mtl_commit_options_add_feedback_handler | async | 7,858 | 220.0 | 0.002 |
| mtl_shared_event_wait_until_signaled_value | sync | 5,898 | 165.1 | 0.589 |
| mtl_encoder_generation | diag | 5,846 | 70.1 | 0.000 |
| mtl_end_command_buffer | async | 3,930 | 47.2 | 0.000 |
| mtl_command_allocator_allocated_size | diag | 3,930 | 47.2 | 0.003 |

## drawstorm (1 windows)

| class | calls/s | wire KB/s | thread-s/s |
|---|---|---|---|
| async | 1,777,532 | 49,166.4 | 0.090 |
| local | 8,588 | 137.4 | 0.001 |
| create | 2,595 | 344.9 | 0.010 |
| diag | 1,717 | 20.6 | 0.001 |
| sync | 870 | 24.2 | 0.313 |

| function (top 14 by calls; r1-r2 logged only top lists, so their rows undercount) | class | calls/s | wire KB/s | thread-s/s |
|---|---|---|---|---|
| mtl_set_address | async | 858,288 | 20,598.9 | 0.016 |
| mtl_draw_primitives | async | 858,288 | 27,465.2 | 0.059 |
| mtl_render_pass_descriptor_get_color_attachment | local | 8,584 | 137.3 | 0.001 |
| mtl_render_pass_attachment_descriptor_set_load_action | async | 8,584 | 137.3 | 0.000 |
| mtl_render_pass_attachment_descriptor_set_store_action | async | 7,726 | 123.6 | 0.000 |
| mtl_render_pass_attachment_descriptor_set_texture | async | 7,726 | 123.6 | 0.000 |
| mtl_render_pass_attachment_descriptor_set_resolve_texture | async | 6,867 | 109.9 | 0.000 |
| mtl_release | async | 2,581 | 31.0 | 0.001 |
| mtl_residency_set_commit | async | 1,716 | 20.6 | 0.000 |
| mtl_residency_set_request_residency | async | 1,716 | 20.6 | 0.004 |
| mtl_commit_options_add_feedback_handler | async | 1,716 | 48.1 | 0.000 |
| mtl_set_cull_mode | async | 858 | 13.7 | 0.000 |
| mtl_begin_command_buffer | async | 858 | 13.7 | 0.001 |
| mtl_render_pass_descriptor_set_visibility_buffer | async | 858 | 13.7 | 0.000 |

## gl-replay-venus (17 windows)

| class | calls/s | wire KB/s | thread-s/s |
|---|---|---|---|
| async | 6,539 | 121.7 | 0.037 |
| local | 730 | 10.8 | 0.000 |
| diag | 477 | 6.4 | 0.000 |
| create | 432 | 41.9 | 0.006 |
| sync | 111 | 3.1 | 0.016 |

| function (top 14 by calls; r1-r2 logged only top lists, so their rows undercount) | class | calls/s | wire KB/s | thread-s/s |
|---|---|---|---|---|
| mtl_render_pass_attachment_descriptor_set_load_action | async | 727 | 11.6 | 0.000 |
| mtl_render_pass_attachment_descriptor_set_store_action | async | 571 | 9.1 | 0.000 |
| mtl_render_pass_attachment_descriptor_set_texture | async | 571 | 9.1 | 0.000 |
| mtl_render_pass_descriptor_get_color_attachment | local | 520 | 8.3 | 0.000 |
| mtl_release | async | 431 | 5.2 | 0.000 |
| mtl_render_pass_attachment_descriptor_set_resolve_texture | async | 416 | 6.7 | 0.000 |
| mtl_commit_options_add_feedback_handler | async | 312 | 8.7 | 0.000 |
| mtl_residency_set_request_residency | async | 311 | 3.7 | 0.002 |
| mtl_residency_set_commit | async | 311 | 3.7 | 0.000 |
| mtl_signal_event | async | 207 | 5.0 | 0.007 |
| mtl_encoder_generation | diag | 160 | 1.9 | 0.000 |
| mtl_begin_command_buffer | async | 156 | 2.5 | 0.001 |
| mtl_end_command_buffer | async | 156 | 1.9 | 0.000 |
| mtl_command_allocator_allocated_size | diag | 156 | 1.9 | 0.000 |

## gl-replay-vrend (17 windows)

| class | calls/s | wire KB/s | thread-s/s |
|---|---|---|---|
| async | 5,213 | 92.4 | 0.011 |
| local | 754 | 11.2 | 0.000 |
| create | 276 | 3.4 | 0.005 |
| diag | 273 | 3.9 | 0.001 |
| sync | 107 | 3.0 | 0.023 |

| function (top 14 by calls; r1-r2 logged only top lists, so their rows undercount) | class | calls/s | wire KB/s | thread-s/s |
|---|---|---|---|---|
| mtl_render_pass_attachment_descriptor_set_load_action | async | 753 | 12.0 | 0.000 |
| mtl_render_pass_attachment_descriptor_set_texture | async | 592 | 9.5 | 0.000 |
| mtl_render_pass_attachment_descriptor_set_store_action | async | 592 | 9.5 | 0.000 |
| mtl_render_pass_descriptor_get_color_attachment | local | 538 | 8.6 | 0.000 |
| mtl_render_pass_attachment_descriptor_set_resolve_texture | async | 430 | 6.9 | 0.000 |
| mtl_release | async | 276 | 3.3 | 0.000 |
| mtl_begin_command_buffer | async | 161 | 2.6 | 0.001 |
| mtl_command_allocator_allocated_size | diag | 161 | 1.9 | 0.000 |
| mtl_end_command_buffer | async | 161 | 1.9 | 0.000 |
| mtl_render_pass_attachment_descriptor_set_level | async | 161 | 2.6 | 0.000 |
| mtl_render_pass_attachment_descriptor_set_slice | async | 161 | 2.6 | 0.000 |
| mtl_set_depth_stencil_state | async | 110 | 1.8 | 0.000 |
| mtl_commit_options_add_feedback_handler | async | 108 | 3.0 | 0.000 |
| mtl_render_pass_descriptor_get_depth_attachment | local | 108 | 1.3 | 0.000 |

## glmark2-venus (6 windows)

| class | calls/s | wire KB/s | thread-s/s |
|---|---|---|---|
| async | 609,891 | 10,931.3 | 0.151 |
| local | 80,352 | 1,245.1 | 0.006 |
| create | 35,225 | 496.2 | 0.082 |
| diag | 16,452 | 197.5 | 0.008 |
| sync | 8,637 | 179.5 | 0.660 |

| function (top 14 by calls; r1-r2 logged only top lists, so their rows undercount) | class | calls/s | wire KB/s | thread-s/s |
|---|---|---|---|---|
| mtl_render_pass_attachment_descriptor_set_load_action | async | 80,346 | 1,285.5 | 0.001 |
| mtl_render_pass_descriptor_get_color_attachment | local | 70,214 | 1,123.4 | 0.005 |
| mtl_render_pass_attachment_descriptor_set_store_action | async | 68,258 | 1,092.1 | 0.001 |
| mtl_render_pass_attachment_descriptor_set_texture | async | 68,258 | 1,092.1 | 0.002 |
| mtl_render_pass_attachment_descriptor_set_resolve_texture | async | 56,171 | 898.7 | 0.001 |
| mtl_release | async | 41,078 | 492.9 | 0.009 |
| mtl_commit_options_add_feedback_handler | async | 18,842 | 527.6 | 0.004 |
| mtl_residency_set_request_residency | async | 14,132 | 169.6 | 0.018 |
| mtl_residency_set_commit | async | 14,131 | 169.6 | 0.005 |
| mtl_render_pass_attachment_descriptor_set_level | async | 12,087 | 193.4 | 0.000 |
| mtl_render_pass_attachment_descriptor_set_slice | async | 12,087 | 193.4 | 0.000 |
| mtl_set_depth_stencil_state | async | 11,736 | 187.8 | 0.000 |
| mtl_render_pass_descriptor_get_depth_attachment | local | 10,132 | 121.6 | 0.001 |
| mtl_begin_command_buffer | async | 9,422 | 150.8 | 0.012 |

## glmark2-vrend (6 windows)

| class | calls/s | wire KB/s | thread-s/s |
|---|---|---|---|
| async | 746,215 | 13,306.5 | 0.129 |
| local | 104,569 | 1,626.5 | 0.007 |
| create | 38,604 | 543.2 | 0.082 |
| diag | 34,182 | 521.7 | 0.031 |
| sync | 17,320 | 374.3 | 0.844 |

| function (top 14 by calls; r1-r2 logged only top lists, so their rows undercount) | class | calls/s | wire KB/s | thread-s/s |
|---|---|---|---|---|
| mtl_render_pass_attachment_descriptor_set_load_action | async | 104,564 | 1,673.0 | 0.001 |
| mtl_render_pass_descriptor_get_color_attachment | local | 92,909 | 1,486.5 | 0.006 |
| mtl_render_pass_attachment_descriptor_set_store_action | async | 89,446 | 1,431.1 | 0.001 |
| mtl_render_pass_attachment_descriptor_set_texture | async | 89,446 | 1,431.1 | 0.002 |
| mtl_render_pass_attachment_descriptor_set_resolve_texture | async | 74,327 | 1,189.2 | 0.001 |
| mtl_release | async | 48,985 | 587.8 | 0.009 |
| mtl_begin_command_buffer | async | 15,595 | 249.5 | 0.025 |
| mtl_end_command_buffer | async | 15,594 | 187.1 | 0.001 |
| mtl_command_allocator_allocated_size | diag | 15,594 | 187.1 | 0.024 |
| mtl_render_pass_attachment_descriptor_set_level | async | 15,118 | 241.9 | 0.000 |
| mtl_render_pass_attachment_descriptor_set_slice | async | 15,118 | 241.9 | 0.000 |
| mtl_set_depth_stencil_state | async | 14,493 | 231.9 | 0.000 |
| mtl_render_pass_descriptor_get_depth_attachment | local | 11,655 | 139.9 | 0.001 |
| mtl_commit_options_add_feedback_handler | async | 11,026 | 308.7 | 0.003 |
