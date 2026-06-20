#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Compile + run vrendprobe.c against the EGL-enabled virglrenderer (third_party/virgl-gl-prefix),
# with zink-on-KK as the GL provider behind virglrenderer's surfaceless EGL winsys. See README.md.
set -euo pipefail
cd "$(dirname "$0")/../.."
REPO="$PWD"

VIRGL_PREFIX="$REPO/third_party/virgl-gl-prefix"
EPOXY_PREFIX="$REPO/third_party/epoxy-egl-prefix"
MESA_PREFIX="${MESA_PREFIX:-/Volumes/mesa-cs/zink-kk-prefix}"
HERE="$REPO/spikes/virgl-zink-kk"

[ -f "$VIRGL_PREFIX/lib/libvirglrenderer.1.dylib" ] || { echo "build-virglrenderer-gl.sh first" >&2; exit 1; }

if [ -z "${VK_DRIVER_FILES:-}" ]; then
  VK_DRIVER_FILES="$(ls /Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.*.json 2>/dev/null | head -1)"
fi
[ -n "${VK_DRIVER_FILES:-}" ] && [ -f "$VK_DRIVER_FILES" ] || { echo "KK ICD not found" >&2; exit 1; }
export VK_DRIVER_FILES

cc -o "$HERE/vrendprobe" "$HERE/vrendprobe.c" \
  -I"$VIRGL_PREFIX/include/virgl" -L"$VIRGL_PREFIX/lib" -lvirglrenderer \
  -Wl,-rpath,"$VIRGL_PREFIX/lib"

# epoxy (loaded by virglrenderer) dlopens bare libEGL/libGLESv2 → must resolve to our zink-kk Mesa;
# also need the vulkan loader for KK. DYLD_* set HERE (after SIP /bin/bash strips it). Force zink→KK.
export DYLD_FALLBACK_LIBRARY_PATH="$MESA_PREFIX/lib:$EPOXY_PREFIX/lib:/opt/homebrew/lib:${DYLD_FALLBACK_LIBRARY_PATH:-}"
export MESA_LOADER_DRIVER_OVERRIDE=zink
export GALLIUM_DRIVER=zink
export LIBGL_DRIVERS_PATH="$MESA_PREFIX/lib"
export EGL_PLATFORM=surfaceless

echo "VK_DRIVER_FILES = $VK_DRIVER_FILES"
echo "virglrenderer   = $VIRGL_PREFIX/lib"
"$HERE/vrendprobe"
