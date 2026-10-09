#!/bin/bash
# Regenerates tess_<stage>.h from the GLSL here (glslangValidator from Homebrew's glslang).
set -euo pipefail
cd "$(dirname "$0")"
for s in vert tesc tese frag; do
  glslangValidator -V --target-env vulkan1.3 --vn "tess_$s" -o "tess_$s.h" "tess.$s" > /dev/null
done
