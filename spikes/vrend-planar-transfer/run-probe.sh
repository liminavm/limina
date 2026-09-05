#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build + run planar-transfer-probe under the same host-GL env the worker gets from
# boot-enhanced-efi-kk.sh (zink-on-KK surfaceless EGL + the KK devenv ICD). Without this
# env virgl_renderer_init aborts on "Couldn't open libEGL.dylib" -- that is the missing
# env, not a virgl failure.
#
# Runs BOTH directions as separate processes: a caught fault leaves the GL driver's state
# untrustworthy, so the second test must not share it.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
MESA_PREFIX="${MESA_PREFIX:-/Volumes/mesa-cs/zink-kk-prefix}"
VIRGL_PREFIX="$ROOT/third_party/virgl-prefix"
VIRGL_SRC="$ROOT/third_party/virglrenderer"

[ -f "$VIRGL_PREFIX/lib/libvirglrenderer.dylib" ] || {
  echo "virgl-prefix missing — run scripts/build-virglrenderer.sh first" >&2; exit 2; }

clang -O0 -g -o "$HERE/planar-transfer-probe" "$HERE/planar-transfer-probe.c" \
  -I"$VIRGL_PREFIX/include/virgl" -I"$VIRGL_SRC/src" \
  -L"$VIRGL_PREFIX/lib" -lvirglrenderer \
  -Wl,-rpath,"$VIRGL_PREFIX/lib" || exit 2

export DYLD_FALLBACK_LIBRARY_PATH="$MESA_PREFIX/lib:$ROOT/third_party/epoxy-egl-prefix/lib:/opt/homebrew/lib${DYLD_FALLBACK_LIBRARY_PATH:+:$DYLD_FALLBACK_LIBRARY_PATH}"
# zink dlopens "@rpath/libvulkan.1.dylib", which resolves against the loader's own rpath
# and not the fallback path; DYLD_LIBRARY_PATH intercepts by leaf name first.
mkdir -p "$MESA_PREFIX/vulkan-rpath"
ln -sf /opt/homebrew/lib/libvulkan.1.dylib "$MESA_PREFIX/vulkan-rpath/libvulkan.1.dylib"
export DYLD_LIBRARY_PATH="$MESA_PREFIX/vulkan-rpath${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}"

export MESA_LOADER_DRIVER_OVERRIDE=zink
export GALLIUM_DRIVER=zink
export LIBGL_DRIVERS_PATH="$MESA_PREFIX/lib"
export EGL_PLATFORM=surfaceless
export VK_ICD_FILENAMES="${VK_ICD_FILENAMES:-/Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.aarch64.json}"

rc=0
for dir in write read; do
  echo "======================================================================"
  "$HERE/planar-transfer-probe" "$dir"
  s=$?
  echo "---- $dir: exit $s ----"
  [ "$s" -eq 0 ] || rc=$s
done
echo "======================================================================"
echo "overall: $rc  (0 = both refused/GREEN, 1 = RED, 2 = probe/env error, 3 = inconclusive)"
exit $rc
