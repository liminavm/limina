#!/bin/bash
# Regenerates shift_comp.h from shift.comp (glslangValidator from Homebrew's glslang).
set -euo pipefail
cd "$(dirname "$0")"
glslangValidator -V --target-env vulkan1.3 --vn shift_comp -o shift_comp.h shift.comp > /dev/null
