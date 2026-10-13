# runs/r4

| workload | windows | KK msg/s | KK wire KB/s | B/msg | KK sync/s | KK bridge thread-s/s | venus cmd/s | venus ring+ctx KB/s | venus thread-s/s | vrend cmd/s | vrend KB/s | msg per venus+vrend cmd | KK wire / stream bytes | KK query round trips/s | KK folded msg/s | folded msg per cmd |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| idle | 0 | 0 | 0.0 | 0.0 | 0.0 | 0.000 | 0 | 0.0 | 0.000 | 0 | 0.0 | 0.00 | 0.00 | 0.0 | 0 | 0.00 |
| aquarium | 11 | 1,711,732 | 61,947.2 | 36.2 | 3,546.0 | 0.875 | 0 | 0.0 | 0.000 | 2,478,112 | 280,606.7 | 0.69 | 0.22 | 1,295.5 | 1,625,739 | 0.66 |

## aquarium (11 windows)

| class | calls/s | wire KB/s | thread-s/s |
|---|---|---|---|
| async | 1,698,791 | 61,717.7 | 0.175 |
| local | 19,890 | 295.8 | 0.003 |
| create | 9,395 | 147.5 | 0.130 |
| diag | 8,112 | 119.6 | 0.009 |
| sync | 3,546 | 81.9 | 0.570 |

| function (top 16 by calls; r1-r2 logged only top lists, so their rows undercount) | class | calls/s | wire KB/s | thread-s/s |
|---|---|---|---|---|
| mtl_set_address | async | 779,400 | 18,705.6 | 0.018 |
| mtl_draw_indexed_primitives | async | 778,733 | 40,494.1 | 0.101 |
| mtl_render_pass_attachment_descriptor_set_load_action | async | 19,311 | 309.0 | 0.000 |
| mtl_render_pass_attachment_descriptor_set_texture | async | 15,364 | 245.8 | 0.001 |
| mtl_render_pass_attachment_descriptor_set_store_action | async | 15,364 | 245.8 | 0.000 |
| mtl_render_pass_descriptor_get_color_attachment | local | 14,272 | 228.4 | 0.002 |
| mtl_render_pass_attachment_descriptor_set_resolve_texture | async | 11,418 | 182.7 | 0.000 |
| mtl_release | async | 9,529 | 114.3 | 0.006 |
| mtl_commit_options_add_feedback_handler | async | 4,999 | 140.0 | 0.002 |
| mtl_render_pass_attachment_descriptor_set_slice | async | 3,947 | 63.1 | 0.000 |
| mtl_render_pass_attachment_descriptor_set_level | async | 3,947 | 63.1 | 0.000 |
| mtl_begin_command_buffer | async | 3,864 | 61.8 | 0.007 |
| mtl_command_allocator_allocated_size | diag | 3,864 | 46.4 | 0.006 |
| mtl_end_command_buffer | async | 3,864 | 46.4 | 0.001 |
| mtl_set_depth_stencil_state | async | 3,629 | 58.1 | 0.000 |
| mtl_render_pass_descriptor_get_depth_attachment | local | 2,535 | 30.4 | 0.000 |
