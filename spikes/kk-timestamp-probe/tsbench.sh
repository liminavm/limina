#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build + run the timestamp cost benchmark against a KK build. See tsbench.c.
#   ./tsbench.sh [iters]        KK_ICD=… to point at a different driver
set -euo pipefail
cd "$(dirname "$0")"
KK_ICD="${KK_ICD:-/Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.aarch64.json}"
[ -f "$KK_ICD" ] || { echo "no KK ICD at $KK_ICD (mount /Volumes/mesa-cs)"; exit 1; }
cc -g -O2 -o tsbench tsbench.c -I/opt/homebrew/include -L/opt/homebrew/lib -lvulkan \
   -Wl,-rpath,/opt/homebrew/lib
VK_ICD_FILENAMES="$KK_ICD" VK_DRIVER_FILES="$KK_ICD" ./tsbench "${1:-2000}"
