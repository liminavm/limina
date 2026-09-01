#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build + run planeimport-probe under the same host-GL env the worker gets from
# boot-enhanced-efi-kk.sh (zink-on-KK surfaceless EGL + the KK devenv ICD).
# Sibling of run-probe.sh; same env, different probe.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
MESA_PREFIX="${MESA_PREFIX:-/Volumes/mesa-cs/zink-kk-prefix}"

clang -O0 -g -o "$HERE/planeimport-probe" "$HERE/planeimport-probe.c" \
  -I"$MESA_PREFIX/include" -L"$MESA_PREFIX/lib" -lEGL -lGLESv2 \
  -framework IOSurface -framework CoreFoundation \
  -Wl,-rpath,"$MESA_PREFIX/lib"

export DYLD_FALLBACK_LIBRARY_PATH="$MESA_PREFIX/lib:$ROOT/third_party/epoxy-egl-prefix/lib:/opt/homebrew/lib${DYLD_FALLBACK_LIBRARY_PATH:+:$DYLD_FALLBACK_LIBRARY_PATH}"
export MESA_LOADER_DRIVER_OVERRIDE=zink
export GALLIUM_DRIVER=zink
export LIBGL_DRIVERS_PATH="$MESA_PREFIX/lib"
export EGL_PLATFORM=surfaceless
export VK_ICD_FILENAMES="${VK_ICD_FILENAMES:-/Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.aarch64.json}"

exec "$HERE/planeimport-probe" "$@"
