#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build zink-msrtt-recursion.c natively on macOS against a zink-on-KK Mesa prefix and run it
# there: zink on the host KosmicKrisp, no VM. Each run is capped and its exit status printed.
#   run-host-kk.sh [runs] [mode...]     default: 3 runs of every mode but fbfetch
# MESA_PREFIX picks the prefix (default the shared /Volumes/mesa-cs/zink-kk-prefix); VK_DRIVER_FILES
# the KK ICD (default the devenv one). A prefix carries its own libvulkan_kosmickrisp.dylib and
# DYLD_LIBRARY_PATH below makes that the one loaded, so a private prefix is a whole A/B arm.
set -uo pipefail

RUNS="${1:-3}"; shift || true
MODES=("$@")
[ ${#MODES[@]} -gt 0 ] || MODES=(color zs flush rebind mrt mrt-rebind mrt3)
PREFIX="${MESA_PREFIX:-/Volumes/mesa-cs/zink-kk-prefix}"
HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="${OUT_DIR:-$HERE}"
BIN="$OUT/zink-msrtt-recursion-host"

if [ -z "${VK_DRIVER_FILES:-}" ]; then
  VK_DRIVER_FILES="$(ls /Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.*.json 2>/dev/null | head -1)"
fi
[ -f "${VK_DRIVER_FILES:-}" ] || { echo "KosmicKrisp ICD not found" >&2; exit 1; }
export VK_DRIVER_FILES VK_ICD_FILENAMES="$VK_DRIVER_FILES"

cc -O2 -o "$BIN" "$HERE/zink-msrtt-recursion.c" -I"$PREFIX/include" -L"$PREFIX/lib" -lEGL -lGLESv2 \
  -Wl,-rpath,"$PREFIX/lib" || exit 1

# dyld strips DYLD_* when it starts SIP-protected /bin/bash, so set them here, for the probe.
mkdir -p "$PREFIX/vulkan-rpath"
ln -sf /opt/homebrew/lib/libvulkan.1.dylib "$PREFIX/vulkan-rpath/libvulkan.1.dylib"
export DYLD_LIBRARY_PATH="$PREFIX/lib:$PREFIX/vulkan-rpath"
export DYLD_FALLBACK_LIBRARY_PATH="/opt/homebrew/lib"
export MESA_LOADER_DRIVER_OVERRIDE=zink GALLIUM_DRIVER=zink LIBGL_DRIVERS_PATH="$PREFIX/lib"
export EGL_PLATFORM=surfaceless

echo "prefix $PREFIX ($(stat -f '%Sm' "$PREFIX"/lib/libgallium-*.dylib)), ICD $VK_DRIVER_FILES"
for m in "${MODES[@]}"; do
  for i in $(seq 1 "$RUNS"); do
    log="$OUT/run-$m-$i.log"
    gtimeout --kill-after=5 60 "$BIN" "$m" > "$log" 2>&1
    rc=$?
    rec=$(grep -c "Caught recursion" "$log")
    echo "== $m #$i exit $rc recursion-lines $rec"
    grep -v "Caught recursion" "$log"
  done
done
