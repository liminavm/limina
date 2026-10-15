#!/bin/bash
# Regenerates span_<name>.h from the GLSL here (glslangValidator from Homebrew's glslang).
set -euo pipefail
cd "$(dirname "$0")"
for s in three skip array inline; do
  glslangValidator -V --target-env vulkan1.3 -S comp --vn "span_$s" -o "span_$s.h" "$s.comp" > /dev/null
done
