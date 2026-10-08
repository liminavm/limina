#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build tesgs.c against a zink-on-KK prefix and run it, with geometry shaders on, for each N.
#   run.sh <kk-build> <N...>     e.g. run.sh /Volumes/mesa-cs/build-kk-tesgs 24 25 30
#   PREFIX=<zink prefix>         default: the shared /Volumes/mesa-cs/zink-kk-prefix
set -euo pipefail
KK="$1"; shift
PREFIX="${PREFIX:-/Volumes/mesa-cs/zink-kk-prefix}"
HERE="$(cd "$(dirname "$0")" && pwd)"
icds=("$KK"/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.*.json)
[ -f "${icds[0]}" ] || { echo "no KosmicKrisp ICD under $KK" >&2; exit 1; }
export VK_DRIVER_FILES="${icds[0]}"
cc -O1 -o "$HERE/tesgs" "$HERE/tesgs.c" -I"$PREFIX/include" -L"$PREFIX/lib" -lEGL \
  -Wl,-rpath,"$PREFIX/lib" -Wl,-rpath,"$(brew --prefix vulkan-loader)/lib"
export MESA_LOADER_DRIVER_OVERRIDE=zink GALLIUM_DRIVER=zink EGL_PLATFORM=surfaceless
export LIMINA_KK_GEOMETRY_SHADER=1 MESA_SHADER_CACHE_DISABLE=true
for n in "$@"; do gtimeout --kill-after=10 60 "$HERE/tesgs" "$n" 2>&1 | grep -v -e '^WARNING' -e LIMINA-CTX -e LIMINA-KK-GUARD; done
