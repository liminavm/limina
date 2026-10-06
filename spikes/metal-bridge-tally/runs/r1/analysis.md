# runs/r1

| workload | windows | KK msg/s | KK wire KB/s | B/msg | KK sync/s | KK bridge thread-s/s | venus cmd/s | venus ring+ctx KB/s | venus thread-s/s | vrend cmd/s | vrend KB/s | msg per venus+vrend cmd | KK wire / stream bytes | KK query round trips/s | KK folded msg/s | folded msg per cmd |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| idle | 0 | 0 | 0.0 | 0.0 | 0.0 | 0.000 | 0 | 0.0 | 0.000 | 0 | 0.0 | 0.00 | 0.00 | 0.0 | 0 | 0.00 |
| vk-replay | 5 | 151,511 | 13,047.9 | 86.1 | 4,376.8 | 0.573 | 44,693 | 3,545.3 | 0.258 | 0 | 0.0 | 3.39 | 3.68 | 496.4 | 70,164 | 1.57 |
| drawstorm | 0 | 0 | 0.0 | 0.0 | 0.0 | 0.000 | 0 | 0.0 | 0.000 | 0 | 0.0 | 0.00 | 0.00 | 0.0 | 0 | 0.00 |
| gl-replay-venus | 7 | 7,207 | 230.0 | 31.9 | 131.6 | 0.077 | 2,810 | 161.4 | 0.053 | 158 | 10.2 | 2.43 | 1.34 | 29.4 | 2,780 | 0.94 |
| gl-replay-vrend | 0 | 0 | 0.0 | 0.0 | 0.0 | 0.000 | 0 | 0.0 | 0.000 | 0 | 0.0 | 0.00 | 0.00 | 0.0 | 0 | 0.00 |
| glmark2-venus | 0 | 0 | 0.0 | 0.0 | 0.0 | 0.000 | 0 | 0.0 | 0.000 | 0 | 0.0 | 0.00 | 0.00 | 0.0 | 0 | 0.00 |
| glmark2-vrend | 0 | 0 | 0.0 | 0.0 | 0.0 | 0.000 | 0 | 0.0 | 0.000 | 0 | 0.0 | 0.00 | 0.00 | 0.0 | 0 | 0.00 |

## vk-replay (5 windows)

| class | calls/s | wire KB/s | thread-s/s |
|---|---|---|---|
| async | 139,856 | 2,654.1 | 0.098 |
| local | 15,427 | 236.1 | 0.002 |
| diag | 10,242 | 138.3 | 0.002 |
| create | 7,279 | 10,272.2 | 0.065 |
| sync | 4,377 | 121.5 | 0.410 |

| function (top 12 by calls; r1-r2 logged only top lists, so their rows undercount) | class | calls/s | wire KB/s | thread-s/s |
|---|---|---|---|---|
| mtl_render_pass_attachment_descriptor_set_load_action | async | 15,306 | 244.9 | 0.000 |
| mtl_render_pass_attachment_descriptor_set_store_action | async | 12,755 | 204.1 | 0.000 |
| mtl_render_pass_descriptor_get_color_attachment | local | 12,755 | 204.1 | 0.002 |
| mtl_render_pass_attachment_descriptor_set_texture | async | 12,755 | 204.1 | 0.001 |
| mtl_render_pass_attachment_descriptor_set_resolve_texture | async | 10,204 | 163.3 | 0.000 |
| mtl_residency_set_commit | async | 7,736 | 92.8 | 0.004 |
| mtl_residency_set_request_residency | async | 7,736 | 92.8 | 0.011 |
| mtl_release | async | 7,307 | 87.7 | 0.007 |
| mtl_signal_event | async | 6,454 | 154.9 | 0.016 |
| mtl_commit_options_add_feedback_handler | async | 5,166 | 144.6 | 0.002 |
| mtl_shared_event_wait_until_signaled_value | sync | 3,878 | 108.6 | 0.410 |
| mtl_encoder_generation | diag | 3,826 | 45.9 | 0.000 |

## gl-replay-venus (7 windows)

| class | calls/s | wire KB/s | thread-s/s |
|---|---|---|---|
| async | 6,618 | 124.5 | 0.031 |
| local | 725 | 10.7 | 0.000 |
| diag | 510 | 6.9 | 0.000 |
| create | 457 | 101.7 | 0.026 |
| sync | 132 | 3.8 | 0.020 |

| function (top 12 by calls; r1-r2 logged only top lists, so their rows undercount) | class | calls/s | wire KB/s | thread-s/s |
|---|---|---|---|---|
| mtl_render_pass_attachment_descriptor_set_load_action | async | 713 | 11.4 | 0.000 |
| mtl_render_pass_attachment_descriptor_set_texture | async | 562 | 9.0 | 0.000 |
| mtl_render_pass_attachment_descriptor_set_store_action | async | 562 | 9.0 | 0.000 |
| mtl_render_pass_descriptor_get_color_attachment | local | 513 | 8.2 | 0.000 |
| mtl_release | async | 452 | 5.4 | 0.000 |
| mtl_render_pass_attachment_descriptor_set_resolve_texture | async | 411 | 6.6 | 0.000 |
| mtl_commit_options_add_feedback_handler | async | 300 | 8.4 | 0.000 |
| mtl_residency_set_request_residency | async | 298 | 3.6 | 0.002 |
| mtl_residency_set_commit | async | 298 | 3.6 | 0.000 |
| mtl_signal_event | async | 199 | 4.8 | 0.005 |
| mtl_command_queue_commit | async | 150 | 3.6 | 0.015 |
| mtl_render_pass_attachment_descriptor_set_slice | async | 113 | 1.8 | 0.000 |
