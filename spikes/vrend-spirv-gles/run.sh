#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build the SPIR-V shaders and the probe, then run it against a zink-on-KK Mesa prefix.
#   run.sh [prefix]   prefix defaults to MESA_PREFIX, else /Volumes/mesa-cs/zink-kk-prefix
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
PREFIX="${1:-${MESA_PREFIX:-/Volumes/mesa-cs/zink-kk-prefix}}"
OUT="${OUT:-$HERE/out}"
mkdir -p "$OUT"

# -G: SPIR-V for OpenGL (ARB_gl_spirv), not Vulkan.
glslangValidator -G -S vert -o "$OUT/tri.vert.spv" "$HERE/tri.vert" >/dev/null
glslangValidator -G -S frag -o "$OUT/tri.frag.spv" "$HERE/tri.frag" >/dev/null
spirv-val --target-env opengl4.5 "$OUT/tri.vert.spv"
spirv-val --target-env opengl4.5 "$OUT/tri.frag.spv"

if [ -z "${VK_DRIVER_FILES:-}" ]; then
  VK_DRIVER_FILES="$(ls /Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.*.json | head -1)"
fi
export VK_DRIVER_FILES
LIBDIR="$PREFIX/lib"
cc -o "$OUT/spirvprobe" "$HERE/spirvprobe.c" -I"$PREFIX/include" -L"$LIBDIR" -lEGL -lGLESv2 \
  -Wl,-rpath,"$LIBDIR"

# As spikes/virgl-zink-kk/run-probe.sh: zink dlopens the Vulkan loader by soname, and the
# DYLD_* must be exported here because dyld strips them from SIP-protected bash.
# zink dlopens "@rpath/libvulkan.1.dylib", which no installed rpath resolves: a directory holding
# only the loader, ahead of rpath resolution, as crates/limina-test's with_virgl_host_gl does.
mkdir -p "$OUT/vulkan-rpath"
ln -sf /opt/homebrew/lib/libvulkan.1.dylib "$OUT/vulkan-rpath/libvulkan.1.dylib"
export DYLD_LIBRARY_PATH="$OUT/vulkan-rpath"
export DYLD_FALLBACK_LIBRARY_PATH="/opt/homebrew/lib:${DYLD_FALLBACK_LIBRARY_PATH:-}"
export MESA_LOADER_DRIVER_OVERRIDE=zink GALLIUM_DRIVER=zink EGL_PLATFORM=surfaceless
export LIBGL_DRIVERS_PATH="$LIBDIR/dri:$LIBDIR"
export MESA_SHADER_CACHE_DISABLE=true

echo "prefix = $PREFIX"
"$OUT/spirvprobe" "$OUT"
