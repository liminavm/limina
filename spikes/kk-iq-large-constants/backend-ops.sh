#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
# Run llama.cpp's test-backend-ops on the host against a KosmicKrisp build: ggml's Vulkan shaders
# go through the same NIR-to-MSL compiler the guest driver uses, without booting a guest.
#
#   spikes/kk-iq-large-constants/backend-ops.sh <kk build dir> <test-backend-ops args...>
#   spikes/kk-iq-large-constants/backend-ops.sh /Volumes/mesa-cs/build-kk \
#       perf -o MUL_MAT -p 'type_a=iq2_s,type_b=f32,m=4096,n=1,k=14336'
#
# Builds llama.cpp at LLAMA_CPP_REV into WORK (default target/llama-cpp-host) on first use; needs
# Homebrew's shaderc (glslc), vulkan-headers and vulkan-loader. Set MESA_KK_DEBUG=msl to see the
# MSL each pipeline compiles to.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
REV="${LLAMA_CPP_REV:-b42b7e6d3059503dd4bed6ca5dcf342bb6379f67}"
WORK="${WORK:-$ROOT/target/llama-cpp-host}"

[ "$#" -ge 2 ] || { echo "usage: $0 <kk build dir> <test-backend-ops args...>" >&2; exit 2; }
KK="$1"; shift
ICD=("$KK"/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.*.json)
[ -e "${ICD[0]}" ] || { echo "no KosmicKrisp ICD under $KK" >&2; exit 2; }
command -v glslc >/dev/null || { echo "missing glslc (brew install shaderc)" >&2; exit 2; }

if [ ! -x "$WORK/build/bin/test-backend-ops" ]; then
  if [ ! -d "$WORK/src/.git" ]; then
    git init -q "$WORK/src"
    git -C "$WORK/src" remote add origin https://github.com/ggml-org/llama.cpp.git
  fi
  git -C "$WORK/src" fetch -q --depth 1 origin "$REV"
  git -C "$WORK/src" checkout -q --detach FETCH_HEAD
  cmake -S "$WORK/src" -B "$WORK/build" -G Ninja -DCMAKE_BUILD_TYPE=Release \
    -DGGML_VULKAN=ON -DGGML_METAL=OFF -DGGML_BLAS=OFF -DLLAMA_CURL=OFF -DLLAMA_BUILD_TESTS=ON \
    -DLLAMA_BUILD_EXAMPLES=OFF -DLLAMA_BUILD_SERVER=OFF -DLLAMA_BUILD_TOOLS=OFF
  ninja -C "$WORK/build" test-backend-ops
fi

VK_ICD_FILENAMES="${ICD[0]}" exec "$WORK/build/bin/test-backend-ops" "$1" -b Vulkan0 "${@:2}"
