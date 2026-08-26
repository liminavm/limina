#!/usr/bin/env bash
# The host KosmicKrisp/zink environment, shared by every host-side vehicle in this spike.
#
# Sourced, not executed. A process launched with NONE of this aborts in GPU init ("Couldn't open
# libEGL.dylib") -- that is the missing env, not a stack fault, and not a reason to reach for
# software-2D.
set -eu
cd "$(dirname "$0")"
ROOT=$(git rev-parse --show-toplevel)
"$ROOT/scripts/ensure-mesa-cs.sh" >/dev/null 2>&1 || true

ICD="${LIMINA_KK_ICD:-$(ls /Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.*.json 2>/dev/null | head -1)}"
[ -n "$ICD" ] && [ -f "$ICD" ] || { echo "no KosmicKrisp ICD under /Volumes/mesa-cs/build-kk"; exit 1; }
MESA_PREFIX="${MESA_PREFIX:-/Volumes/mesa-cs/zink-kk-prefix}"

export VK_ICD_FILENAMES="$ICD"
export VK_DRIVER_FILES="$ICD"
# zink dlopens "@rpath/libvulkan.1.dylib"; DYLD_LIBRARY_PATH intercepts by leaf name.
mkdir -p "$MESA_PREFIX/vulkan-rpath"
ln -sf /opt/homebrew/lib/libvulkan.1.dylib "$MESA_PREFIX/vulkan-rpath/libvulkan.1.dylib"
export DYLD_LIBRARY_PATH="$MESA_PREFIX/vulkan-rpath${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}"
export DYLD_FALLBACK_LIBRARY_PATH="$MESA_PREFIX/lib:/opt/homebrew/lib${DYLD_FALLBACK_LIBRARY_PATH:+:$DYLD_FALLBACK_LIBRARY_PATH}"
export MESA_LOADER_DRIVER_OVERRIDE="${LIMINA_HOST_GALLIUM:-zink}"
export GALLIUM_DRIVER="${LIMINA_HOST_GALLIUM:-zink}"
export LIBGL_DRIVERS_PATH="$MESA_PREFIX/lib"
export EGL_PLATFORM=surfaceless

