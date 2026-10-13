# runs/r2

| workload | windows | KK msg/s | KK wire KB/s | B/msg | KK sync/s | KK bridge thread-s/s | venus cmd/s | venus ring+ctx KB/s | venus thread-s/s | vrend cmd/s | vrend KB/s | msg per venus+vrend cmd | KK wire / stream bytes | KK query round trips/s | KK folded msg/s | folded msg per cmd |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| idle | 0 | 0 | 0.0 | 0.0 | 0.0 | 0.000 | 0 | 0.0 | 0.000 | 0 | 0.0 | 0.00 | 0.00 | 0.0 | 0 | 0.00 |
| vk-replay | 5 | 227,139 | 19,574.4 | 86.2 | 6,561.6 | 0.774 | 66,577 | 4,885.3 | 0.314 | 0 | 0.0 | 3.41 | 4.01 | 750.4 | 105,882 | 1.59 |
| drawstorm | 1 | 1,708,295 | 47,209.8 | 27.6 | 823.0 | 0.419 | 1,445,290 | 42.6 | 0.465 | 0 | 0.0 | 1.18 | 1107.28 | 0.0 | 4,955,267 | 3.43 |
| gl-replay-venus | 18 | 6,781 | 161.3 | 23.8 | 109.7 | 0.063 | 2,769 | 159.4 | 0.056 | 61 | 3.5 | 2.40 | 0.99 | 10.6 | 2,785 | 0.98 |
| gl-replay-vrend | 27 | 3,482 | 61.5 | 17.7 | 62.9 | 0.068 | 0 | 0.0 | 0.000 | 285 | 151.3 | 12.23 | 0.41 | 2.3 | 942 | 3.31 |
| glmark2-venus | 6 | 809,397 | 14,360.2 | 17.7 | 11,851.8 | 1.020 | 261,500 | 13,597.4 | 0.502 | 26 | 1.2 | 3.09 | 1.06 | 13,809.5 | 276,298 | 1.06 |
| glmark2-vrend | 6 | 738,453 | 13,095.8 | 17.7 | 15,918.0 | 1.033 | 0 | 0.0 | 0.000 | 65,017 | 22,829.4 | 11.36 | 0.57 | 10,199.5 | 246,712 | 3.79 |

## vk-replay (5 windows)

| class | calls/s | wire KB/s | thread-s/s |
|---|---|---|---|
| async | 209,664 | 3,978.9 | 0.123 |
| local | 23,127 | 354.0 | 0.002 |
| diag | 15,354 | 207.4 | 0.003 |
| create | 10,914 | 15,413.4 | 0.070 |
| sync | 6,562 | 182.2 | 0.582 |

| function (top 12 by calls; r1-r2 logged only top lists, so their rows undercount) | class | calls/s | wire KB/s | thread-s/s |
|---|---|---|---|---|
| mtl_render_pass_attachment_descriptor_set_load_action | async | 22,945 | 367.1 | 0.000 |
| mtl_render_pass_attachment_descriptor_set_store_action | async | 19,121 | 305.9 | 0.000 |
| mtl_render_pass_descriptor_get_color_attachment | local | 19,121 | 305.9 | 0.002 |
| mtl_render_pass_attachment_descriptor_set_texture | async | 19,121 | 305.9 | 0.001 |
| mtl_render_pass_attachment_descriptor_set_resolve_texture | async | 15,296 | 244.7 | 0.000 |
| mtl_residency_set_commit | async | 11,597 | 139.2 | 0.004 |
| mtl_residency_set_request_residency | async | 11,597 | 139.2 | 0.016 |
| mtl_release | async | 10,958 | 131.5 | 0.009 |
| mtl_signal_event | async | 9,675 | 232.2 | 0.021 |
| mtl_commit_options_add_feedback_handler | async | 7,744 | 216.8 | 0.003 |
| mtl_shared_event_wait_until_signaled_value | sync | 5,813 | 162.8 | 0.581 |
| mtl_encoder_generation | diag | 5,736 | 68.8 | 0.000 |

## drawstorm (1 windows)

| class | calls/s | wire KB/s | thread-s/s |
|---|---|---|---|
| async | 1,705,001 | 47,160.4 | 0.090 |
| local | 8,234 | 131.7 | 0.001 |
| create | 2,470 | 26.3 | 0.012 |
| diag | 1,646 | 19.8 | 0.001 |
| sync | 823 | 23.1 | 0.316 |

| function (top 12 by calls; r1-r2 logged only top lists, so their rows undercount) | class | calls/s | wire KB/s | thread-s/s |
|---|---|---|---|---|
| mtl_draw_primitives | async | 823,271 | 26,344.7 | 0.055 |
| mtl_set_address | async | 823,271 | 19,758.5 | 0.015 |
| mtl_render_pass_attachment_descriptor_set_load_action | async | 8,234 | 131.7 | 0.000 |
| mtl_render_pass_descriptor_get_color_attachment | local | 8,234 | 131.7 | 0.001 |
| mtl_render_pass_attachment_descriptor_set_store_action | async | 7,411 | 118.6 | 0.000 |
| mtl_render_pass_attachment_descriptor_set_texture | async | 7,411 | 118.6 | 0.000 |
| mtl_render_pass_attachment_descriptor_set_resolve_texture | async | 6,587 | 105.4 | 0.000 |
| mtl_release | async | 2,470 | 29.6 | 0.001 |
| mtl_residency_set_commit | async | 1,647 | 19.8 | 0.000 |
| mtl_residency_set_request_residency | async | 1,647 | 19.8 | 0.005 |
| mtl_commit_options_add_feedback_handler | async | 1,646 | 46.1 | 0.001 |
| mtl_set_visibility_result_mode | async | 823 | 19.8 | 0.000 |

## gl-replay-venus (18 windows)

| class | calls/s | wire KB/s | thread-s/s |
|---|---|---|---|
| async | 6,257 | 116.9 | 0.033 |
| local | 699 | 10.4 | 0.000 |
| diag | 464 | 6.2 | 0.000 |
| create | 414 | 41.4 | 0.009 |
| sync | 110 | 3.1 | 0.022 |

| function (top 12 by calls; r1-r2 logged only top lists, so their rows undercount) | class | calls/s | wire KB/s | thread-s/s |
|---|---|---|---|---|
| mtl_render_pass_attachment_descriptor_set_load_action | async | 695 | 11.1 | 0.000 |
| mtl_render_pass_attachment_descriptor_set_texture | async | 546 | 8.7 | 0.000 |
| mtl_render_pass_attachment_descriptor_set_store_action | async | 546 | 8.7 | 0.000 |
| mtl_render_pass_descriptor_get_color_attachment | local | 498 | 8.0 | 0.000 |
| mtl_release | async | 412 | 4.9 | 0.000 |
| mtl_render_pass_attachment_descriptor_set_resolve_texture | async | 398 | 6.4 | 0.000 |
| mtl_commit_options_add_feedback_handler | async | 296 | 8.3 | 0.000 |
| mtl_residency_set_commit | async | 295 | 3.5 | 0.000 |
| mtl_residency_set_request_residency | async | 295 | 3.5 | 0.005 |
| mtl_signal_event | async | 197 | 4.7 | 0.006 |
| mtl_command_queue_commit | async | 148 | 3.5 | 0.013 |
| mtl_begin_command_buffer | async | 140 | 2.2 | 0.001 |

## gl-replay-vrend (27 windows)

| class | calls/s | wire KB/s | thread-s/s |
|---|---|---|---|
| async | 3,246 | 57.6 | 0.045 |
| local | 468 | 7.0 | 0.000 |
| create | 173 | 2.1 | 0.010 |
| diag | 170 | 2.5 | 0.001 |
| sync | 63 | 1.7 | 0.013 |

| function (top 12 by calls; r1-r2 logged only top lists, so their rows undercount) | class | calls/s | wire KB/s | thread-s/s |
|---|---|---|---|---|
| mtl_render_pass_attachment_descriptor_set_load_action | async | 467 | 7.5 | 0.000 |
| mtl_render_pass_attachment_descriptor_set_store_action | async | 367 | 5.9 | 0.000 |
| mtl_render_pass_attachment_descriptor_set_texture | async | 367 | 5.9 | 0.000 |
| mtl_render_pass_descriptor_get_color_attachment | local | 334 | 5.3 | 0.000 |
| mtl_render_pass_attachment_descriptor_set_resolve_texture | async | 267 | 4.3 | 0.000 |
| mtl_release | async | 173 | 2.1 | 0.001 |
| mtl_begin_command_buffer | async | 100 | 1.6 | 0.004 |
| mtl_end_command_buffer | async | 100 | 1.2 | 0.000 |
| mtl_command_allocator_allocated_size | diag | 100 | 1.2 | 0.001 |
| mtl_render_pass_attachment_descriptor_set_slice | async | 100 | 1.6 | 0.000 |
| mtl_render_pass_attachment_descriptor_set_level | async | 100 | 1.6 | 0.000 |
| mtl_set_depth_stencil_state | async | 69 | 1.1 | 0.000 |

## glmark2-venus (6 windows)

| class | calls/s | wire KB/s | thread-s/s |
|---|---|---|---|
| async | 755,626 | 13,522.0 | 0.165 |
| local | 101,292 | 1,576.1 | 0.006 |
| create | 41,919 | 616.5 | 0.121 |
| diag | 18,904 | 226.9 | 0.007 |
| sync | 11,852 | 221.7 | 0.735 |

| function (top 12 by calls; r1-r2 logged only top lists, so their rows undercount) | class | calls/s | wire KB/s | thread-s/s |
|---|---|---|---|---|
| mtl_render_pass_attachment_descriptor_set_load_action | async | 101,286 | 1,620.6 | 0.001 |
| mtl_render_pass_descriptor_get_color_attachment | local | 90,153 | 1,442.4 | 0.005 |
| mtl_render_pass_attachment_descriptor_set_texture | async | 86,704 | 1,387.3 | 0.002 |
| mtl_render_pass_attachment_descriptor_set_store_action | async | 86,704 | 1,387.3 | 0.001 |
| mtl_render_pass_attachment_descriptor_set_resolve_texture | async | 72,122 | 1,154.0 | 0.001 |
| mtl_release | async | 52,251 | 627.0 | 0.024 |
| mtl_commit_options_add_feedback_handler | async | 18,830 | 527.2 | 0.004 |
| mtl_residency_set_commit | async | 14,819 | 177.8 | 0.005 |
| mtl_residency_set_request_residency | async | 14,122 | 169.5 | 0.017 |
| mtl_set_depth_stencil_state | async | 13,959 | 223.3 | 0.000 |
| mtl_render_pass_attachment_descriptor_set_slice | async | 12,439 | 199.0 | 0.000 |
| mtl_render_pass_attachment_descriptor_set_level | async | 10,546 | 168.7 | 0.000 |

## glmark2-vrend (6 windows)

| class | calls/s | wire KB/s | thread-s/s |
|---|---|---|---|
| async | 686,712 | 12,247.9 | 0.124 |
| local | 96,010 | 1,491.6 | 0.006 |
| create | 35,824 | 495.3 | 0.092 |
| diag | 32,098 | 487.0 | 0.028 |
| sync | 15,918 | 352.5 | 0.816 |

| function (top 12 by calls; r1-r2 logged only top lists, so their rows undercount) | class | calls/s | wire KB/s | thread-s/s |
|---|---|---|---|---|
| mtl_render_pass_attachment_descriptor_set_load_action | async | 96,006 | 1,536.1 | 0.001 |
| mtl_render_pass_descriptor_get_color_attachment | local | 84,866 | 1,357.9 | 0.006 |
| mtl_render_pass_attachment_descriptor_set_texture | async | 81,949 | 1,311.2 | 0.002 |
| mtl_render_pass_attachment_descriptor_set_store_action | async | 81,949 | 1,311.2 | 0.001 |
| mtl_render_pass_attachment_descriptor_set_resolve_texture | async | 67,892 | 1,086.3 | 0.001 |
| mtl_release | async | 44,567 | 534.8 | 0.009 |
| mtl_begin_command_buffer | async | 14,525 | 232.4 | 0.022 |
| mtl_end_command_buffer | async | 14,525 | 174.3 | 0.001 |
| mtl_command_allocator_allocated_size | diag | 14,525 | 174.3 | 0.020 |
| mtl_render_pass_attachment_descriptor_set_level | async | 14,056 | 224.9 | 0.000 |
| mtl_render_pass_attachment_descriptor_set_slice | async | 14,056 | 224.9 | 0.000 |
| mtl_set_depth_stencil_state | async | 13,530 | 216.5 | 0.000 |
