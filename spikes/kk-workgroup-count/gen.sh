#!/bin/bash
# Regenerates count_comp.h from count.comp (glslangValidator from Homebrew's glslang).
set -euo pipefail
cd "$(dirname "$0")"
glslangValidator -V --target-env vulkan1.3 --vn count_comp -o count_comp.h count.comp > /dev/null
