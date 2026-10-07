#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build blendadv.c against a zink-on-KK prefix and run it on the KosmicKrisp ICD.
#   run.sh [prefix] [kk-build]   defaults: the shared builds on /Volumes/mesa-cs
set -euo pipefail
PREFIX="${1:-/Volumes/mesa-cs/zink-kk-prefix}"
KK="${2:-/Volumes/mesa-cs/build-kk}"
HERE="$(cd "$(dirname "$0")" && pwd)"
icds=("$KK"/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.*.json)
[ -f "${icds[0]}" ] || { echo "no KosmicKrisp ICD under $KK" >&2; exit 1; }
export VK_DRIVER_FILES="${icds[0]}"
cc -O1 -o "$HERE/blendadv" "$HERE/blendadv.c" -I"$PREFIX/include" -L"$PREFIX/lib" -lEGL \
  -Wl,-rpath,"$PREFIX/lib" -Wl,-rpath,"$(brew --prefix vulkan-loader)/lib"
export MESA_LOADER_DRIVER_OVERRIDE=zink GALLIUM_DRIVER=zink EGL_PLATFORM=surfaceless
"$HERE/blendadv"
