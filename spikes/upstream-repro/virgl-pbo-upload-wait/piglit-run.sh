#!/bin/bash
# piglit-run.sh <build: main|fix> <results-dir>
# Buffer mapping, PBO, texture upload/download and host-written buffer groups:
# everything a read-only map of a buffer or texture can reach.
set -u
cd ~/piglit
export PIGLIT_PLATFORM=surfaceless_egl PIGLIT_BUILD_DIR=$HOME/piglit/build
mesa-env "$1" ./piglit run quick -c -o \
  -t 'arb_pixel_buffer_object' -t 'arb_map_buffer_range' -t 'arb_buffer_storage' \
  -t 'arb_copy_buffer' -t 'arb_vertex_buffer_object' -t 'arb_query_buffer_object' \
  -t 'arb_shader_storage_buffer_object' -t 'arb_shader_image_load_store' \
  -t 'transform_feedback' -t 'arb_texture_buffer_object' -t 'arb_texture_buffer_range' \
  -t 'arb_get_texture_sub_image' -t 'arb_direct_state_access' -t 'arb_uniform_buffer_object' \
  -t 'arb_clear_buffer_object' -t 'arb_draw_indirect' -t 'arb_indirect_parameters' \
  -t 'texsubimage' -t 'teximage' -t 'getteximage' -t 'readpixels' -t 'pbo' -t 'bufferobj' \
  "$2" > "$2.log" 2>&1
echo "exit=$?"
