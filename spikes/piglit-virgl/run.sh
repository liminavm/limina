#!/bin/bash
# run.sh <results-dir> — run piglit's buffer, PBO and texture-transfer groups in a guest.
# The same test selection as spikes/upstream-repro/virgl-pbo-upload-wait/piglit-run.sh, so the
# two rigs compare test for test. gbm, not surfaceless_egl: tests that draw to the default
# framebuffer get one, where surfaceless makes piglit abort them in run_test.
# Serial (-1) and fsynced (-s), so a test that takes the VM down is the one left incomplete, and
# that survives the crash. Resume with: ./piglit resume --no-retry <results-dir> (drive.sh does).
set -u
cd ~/piglit
export PIGLIT_PLATFORM=${PIGLIT_PLATFORM:-gbm}
./piglit run quick -1 -s --timeout 300 -o \
  -t 'arb_pixel_buffer_object' -t 'arb_map_buffer_range' -t 'arb_buffer_storage' \
  -t 'arb_copy_buffer' -t 'arb_vertex_buffer_object' -t 'arb_query_buffer_object' \
  -t 'arb_shader_storage_buffer_object' -t 'arb_shader_image_load_store' \
  -t 'transform_feedback' -t 'arb_texture_buffer_object' -t 'arb_texture_buffer_range' \
  -t 'arb_get_texture_sub_image' -t 'arb_direct_state_access' -t 'arb_uniform_buffer_object' \
  -t 'arb_clear_buffer_object' -t 'arb_draw_indirect' -t 'arb_indirect_parameters' \
  -t 'texsubimage' -t 'teximage' -t 'getteximage' -t 'readpixels' -t 'pbo' -t 'bufferobj' \
  "$1" > "$1.log" 2>&1
echo "exit=$?"
