#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# Build + run the zink shadow-attachment recursion oracle against HOST zink-on-KK.
#
# No VM in the loop: the recursion is in zink, which is the same code in the guest
# (zink-on-venus) and on the host (zink-on-KK, what vrend renders through). The
# host is the fast edit/build/test loop; confirm in a guest once at the end.
#
# The host exposure is the worse one, incidentally: a guest GL app that trips this
# through the vrend tier overflows the WORKER's stack, taking down the whole VM
# rather than one guest process.
#
# Env mirrors spikes/venus-draw-probe/boot-enhanced-efi-kk.sh (see it for why each
# selector is needed).
set -euo pipefail
cd "$(dirname "$0")"

MESA_PREFIX="${MESA_PREFIX:-/Volumes/mesa-cs/zink-kk-prefix}"
[ -d "$MESA_PREFIX/lib" ] || {
    echo "no zink-on-KK prefix at $MESA_PREFIX (mount mesa-cs and build it)" >&2
    exit 77
}

ICD="${LIMINA_KK_ICD:-$(ls /Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.*.json 2>/dev/null | head -1)}"
[ -n "$ICD" ] && [ -f "$ICD" ] || {
    echo "no KosmicKrisp ICD under /Volumes/mesa-cs/build-kk" >&2
    exit 77
}

cc -O0 -g -o shadow-recursion shadow-recursion.c \
    -I"$MESA_PREFIX/include" -L"$MESA_PREFIX/lib" -lEGL -lGLESv2 \
    -Wl,-rpath,"$MESA_PREFIX/lib"

export VK_ICD_FILENAMES="$ICD"
export VK_DRIVER_FILES="$ICD"
export DYLD_FALLBACK_LIBRARY_PATH="$MESA_PREFIX/lib:/opt/homebrew/lib${DYLD_FALLBACK_LIBRARY_PATH:+:$DYLD_FALLBACK_LIBRARY_PATH}"
# zink dlopens "@rpath/libvulkan.1.dylib" since the MTL4 rebase and the installed
# libgallium has no matching LC_RPATH; a shim dir holding ONLY that leaf name
# resolves it without shadowing every Homebrew library for the process.
mkdir -p "$MESA_PREFIX/vulkan-rpath"
ln -sf /opt/homebrew/lib/libvulkan.1.dylib "$MESA_PREFIX/vulkan-rpath/libvulkan.1.dylib"
export DYLD_LIBRARY_PATH="$MESA_PREFIX/vulkan-rpath${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}"
export MESA_LOADER_DRIVER_OVERRIDE=zink
export GALLIUM_DRIVER=zink
export LIBGL_DRIVERS_PATH="$MESA_PREFIX/lib"
export EGL_PLATFORM=surfaceless

# A stack overflow from unbounded recursion can take a while to arrive and prints
# thousands of u_blitter recursion warnings on the way; cap it so a RED run ends.
exec ${SHADOW_RECURSION_RUNNER:-} ./shadow-recursion "$@"
