#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Compile + run a probe against the spike's zink-on-KK Mesa, forcing the zink gallium driver
# onto the KosmicKrisp Vulkan ICD via SURFACELESS EGL. See README.md.
#   run-probe.sh [probe]   probe = eglprobe (default, GLES clear) | glprobe (desktop-GL tex draw)
set -euo pipefail

PROBE="${1:-eglprobe}"
PREFIX="${MESA_PREFIX:-/Volumes/mesa-cs/zink-kk-prefix}"
HERE="$(cd "$(dirname "$0")" && pwd)"

# KK ICD (host Vulkan backend zink runs on). Resolve the devenv ICD or honor an override.
if [ -z "${VK_DRIVER_FILES:-}" ]; then
  VK_DRIVER_FILES="$(ls /Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.*.json 2>/dev/null | head -1)"
fi
[ -n "${VK_DRIVER_FILES:-}" ] && [ -f "$VK_DRIVER_FILES" ] || {
  echo "KosmicKrisp ICD not found — build KK first (/Volumes/mesa-cs/build-kk)" >&2; exit 1; }
export VK_DRIVER_FILES

LIBDIR="$(find "$PREFIX" -name 'libEGL*.dylib' -maxdepth 3 2>/dev/null | head -1 | xargs dirname)"
[ -n "$LIBDIR" ] || { echo "libEGL not found under $PREFIX — run build-mesa-zink-kk.sh" >&2; exit 1; }

# GL entry points are loaded via eglGetProcAddress (glprobe) so we only ever link libEGL
# (+ libGLESv2 for eglprobe's direct ES calls). No libGL — glx/glvnd are disabled in the build.
EXTRA_LIBS="-lGLESv2"
[ "$PROBE" = glprobe ] && EXTRA_LIBS=""
cc -o "$HERE/$PROBE" "$HERE/$PROBE.c" \
  -I"$PREFIX/include" -L"$LIBDIR" -lEGL $EXTRA_LIBS \
  -Wl,-rpath,"$LIBDIR"

# zink dlopen()s the bare soname "libvulkan.1.dylib" (zink_screen.c). The Homebrew loader
# isn't on dyld's default search path, so point DYLD_FALLBACK_LIBRARY_PATH at it. NOTE: this
# MUST be exported here (inside the script), not on the outer command line — dyld strips DYLD_*
# when launching SIP-protected /bin/bash, so it'd never reach the probe otherwise. The probe is
# an unrestricted binary, so DYLD_* set by bash is honored for it.
export DYLD_FALLBACK_LIBRARY_PATH="/opt/homebrew/lib:${DYLD_FALLBACK_LIBRARY_PATH:-}"

# Force zink onto KK; surfaceless platform; point the DRI loader at our gallium build.
export MESA_LOADER_DRIVER_OVERRIDE=zink
export GALLIUM_DRIVER=zink
export LIBGL_DRIVERS_PATH="$LIBDIR/dri:$LIBDIR"
export EGL_PLATFORM=surfaceless
export __EGL_VENDOR_LIBRARY_DIRS="$PREFIX/share/glvnd/egl_vendor.d"

echo "VK_DRIVER_FILES = $VK_DRIVER_FILES"
echo "libEGL          = $LIBDIR  (probe=$PROBE)"
"$HERE/$PROBE"
