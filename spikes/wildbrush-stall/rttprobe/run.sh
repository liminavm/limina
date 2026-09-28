#!/bin/sh
# Build and run the render-pass split probes on host zink-on-KosmicKrisp: ping-pong with and
# without the eager end-of-pass barrier, and a depth pass whose depth/stencil goes in and out of
# use. Needs no VM. Prints the pixel verdict and the [LIMINA-ZINK-RP] resume counts of each
# arm.
#
# The round counts are NOT a throughput measure. Every round ends in a synchronous glReadPixels,
# so they measure a round trip, and they are non-monotonic in draws per pass: one pass of 1000
# draws costs ~1 ms per round, one of 2000 ~2.3 ms. Judge performance on a real workload. This
# probe is the pixel and pass-count oracle.
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
F='renderer|rounds|wrong|LIMINA-ZINK-RP\] ctx|resume|split'
echo "== ping-pong, eager end-of-pass barrier (default)"
./rttprobe 2>&1 | grep -E "$F" || true
echo "== ping-pong, without it (LIMINA_ZINK_NO_EAGER_RP_BARRIER=1)"
LIMINA_ZINK_NO_EAGER_RP_BARRIER=1 ./rttprobe 2>&1 | grep -E "$F" || true
echo "== depth/stencil going in and out of use within a pass"
./rttprobe depth 2>&1 | grep -E "$F" || true
