#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# The compute battery's native control, built into $PERF_HOME from the same pins as the tools disk:
#   native/metal-bench   metal-bench.cpp against Metal.framework (metal-cpp from the shim spike)
#   venv-native          MLX's Python package from MLX_TREE at its HEAD, and mlx-lm MLX_LM
#   llama-cpp-host       llama-bench at LLAMA_CPP_REV with its Vulkan backend, run on the host's
#                        KosmicKrisp (Homebrew's shaderc compiles its shaders: record its version)
# The MLX and llama.cpp steps are skipped when their output names the pin it was built from.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(git -C "$HERE" rev-parse --show-toplevel)
. "$HERE/env.sh"
N=$PERF_HOME/native
mkdir -p "$N"

# metal-bench is small: always rebuilt, so it is this tree's.
[ -d "$ROOT/spikes/metal-cpp-shim/out/metal-cpp" ] || "$ROOT/spikes/metal-cpp-shim/build.sh"
xcrun -sdk macosx metal -o "$N/metal-bench.metallib" "$HERE/metal-bench.metal"
clang++ -std=c++17 -O2 -I "$ROOT/spikes/metal-cpp-shim/out/metal-cpp" "$HERE/metal-bench.cpp" \
  -framework Metal -framework Foundation -framework QuartzCore -o "$N/metal-bench"

V=$PERF_HOME/venv-native
MLX_REV=$(git -C "$MLX_TREE" rev-parse HEAD)
if [ "$(cat "$V/MLX_REV" 2>/dev/null)" != "$MLX_REV $MLX_LM" ]; then
  [ -x "$V/bin/python3" ] || python3 -m venv "$V"
  rm -rf "${PERF_HOME:?}/mlx-src"
  mkdir -p "$PERF_HOME/mlx-src"
  git -C "$MLX_TREE" archive HEAD | tar -x -C "$PERF_HOME/mlx-src"
  "$V/bin/pip" install -q setuptools typing_extensions numpy nanobind cmake huggingface_hub
  (cd "$PERF_HOME/mlx-src" && DEV_RELEASE=1 "$V/bin/pip" install -q --no-build-isolation .)
  "$V/bin/pip" install -q --no-deps "mlx-lm==$MLX_LM"
  # mlx-lm's own requirements, without the mlx one pip would take from PyPI.
  # shellcheck disable=SC2046 # one word per requirement
  "$V/bin/pip" install -q $("$V/bin/python3" -c 'import importlib.metadata as m; print(" ".join(r.split(";")[0].replace(" ", "") for r in m.requires("mlx-lm") if "extra ==" not in r and not r.startswith("mlx")))')
  echo "$MLX_REV $MLX_LM" > "$V/MLX_REV"
fi

L=$PERF_HOME/llama-cpp-host
if [ "$(cat "$L/REV" 2>/dev/null)" != "$LLAMA_CPP_REV" ]; then
  "$HERE/llama-src.sh"
  rm -rf "${PERF_HOME:?}/llama-cpp-host"
  mkdir -p "$L"
  tar -C "$L" -xf "$PERF_HOME/llama.cpp-src.tar"
  cmake -S "$L/src" -B "$L/build" -G Ninja -DCMAKE_BUILD_TYPE=Release \
    -DGGML_VULKAN=ON -DGGML_METAL=OFF -DGGML_BLAS=OFF -DLLAMA_CURL=OFF -DLLAMA_BUILD_TESTS=OFF \
    -DLLAMA_BUILD_EXAMPLES=OFF -DLLAMA_BUILD_SERVER=OFF -DLLAMA_BUILD_TOOLS=ON > "$L/configure.log"
  ninja -C "$L/build" llama-bench > "$L/build.log"
  echo "$LLAMA_CPP_REV" > "$L/REV"
fi
echo "native control ready in $PERF_HOME"
