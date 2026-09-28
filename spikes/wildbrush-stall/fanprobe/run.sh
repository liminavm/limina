#!/bin/sh
# Build and run the fan oracle against the local KosmicKrisp build, with and without the
# static-index fast path. Needs no VM.
set -e
cd "$(dirname "$0")"
OUT=${OUT:-/tmp/fanprobe}
mkdir -p "$OUT"
glslangValidator -V fan.vert -o "$OUT/fan.vert.spv" >/dev/null
glslangValidator -V fan.frag -o "$OUT/fan.frag.spv" >/dev/null
cc -O1 -I/opt/homebrew/include fanprobe.c -L/opt/homebrew/lib -lvulkan -lm -o "$OUT/fanprobe"
export VK_ICD_FILENAMES=/Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.aarch64.json
cd "$OUT"
echo "== static fan indices (default)"; ./fanprobe
echo "== GPU unroll (LIMINA_KK_NO_FAN_STATIC=1)"; LIMINA_KK_NO_FAN_STATIC=1 ./fanprobe
