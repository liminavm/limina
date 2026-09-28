#!/bin/sh
# Build and run the render-to-texture split probe on host zink-on-KosmicKrisp, with the eager
# end-of-pass barrier (default) and without it. Needs no VM. Prints the pixel verdict and the
# [LIMINA-ZINK-RP] resume counts of each arm.
set -e
cd "$(dirname "$0")"
OUT=${OUT:-/tmp/rttprobe}
PREFIX=/Volumes/mesa-cs/zink-kk-prefix
mkdir -p "$OUT"
cc -O1 -I"$PREFIX/include" rttprobe.c -L"$PREFIX/lib" -lEGL -lGLESv2 -o "$OUT/rttprobe"
export VK_ICD_FILENAMES=/Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.aarch64.json
export VK_DRIVER_FILES="$VK_ICD_FILENAMES"
export DYLD_FALLBACK_LIBRARY_PATH="$PREFIX/lib:/opt/homebrew/lib"
export DYLD_LIBRARY_PATH="$PREFIX/vulkan-rpath"
export MESA_LOADER_DRIVER_OVERRIDE=zink GALLIUM_DRIVER=zink LIBGL_DRIVERS_PATH="$PREFIX/lib"
export EGL_PLATFORM=surfaceless LIMINA_ZINK_RP_STATS=1
cd "$OUT"
echo "== eager end-of-pass barrier (default)"
./rttprobe 2>&1 | grep -E "renderer|rounds|wrong|LIMINA-ZINK-RP\] ctx|resume|split" || true
echo "== without it (LIMINA_ZINK_NO_EAGER_RP_BARRIER=1)"
LIMINA_ZINK_NO_EAGER_RP_BARRIER=1 ./rttprobe 2>&1 | grep -E "renderer|rounds|wrong|LIMINA-ZINK-RP\] ctx|resume|split" || true
