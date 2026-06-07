#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build + run the standalone venus context-create reproduction against our
# in-repo upstream virglrenderer 1.3.0 build (third_party/virgl-prefix).
#
# Usage:
#   bash run.sh          # build and run
#   bash run.sh lldb     # build and run under lldb (catch the crash/exit)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
PREFIX="$ROOT/third_party/virgl-prefix"
HERE="$ROOT/spikes/venus-render-server"
BIN="$HERE/harness"

if [[ ! -f "$PREFIX/lib/libvirglrenderer.1.dylib" ]]; then
  echo "missing $PREFIX — build with scripts/build-virglrenderer.sh first" >&2
  exit 1
fi

clang -g -O0 \
  -I"$PREFIX/include/virgl" \
  "$HERE/harness.c" \
  -L"$PREFIX/lib" -lvirglrenderer \
  -o "$BIN"

# The dylib install_name is absolute, so no DYLD path needed. Surface every
# virglrenderer log. MoltenVK verbose so we SEE Metal init in the worker thread.
export VIRGL_LOG_LEVEL=debug
export MVK_CONFIG_LOG_LEVEL=3
export VK_ICD_FILENAMES="$(brew --prefix)/opt/molten-vk/share/vulkan/icd.d/MoltenVK_icd.json"

if [[ "${1:-}" == "lldb" ]]; then
  exec lldb -o run -o "bt all" -o "quit" -- "$BIN"
else
  exec "$BIN"
fi
