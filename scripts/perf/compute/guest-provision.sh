#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# The guest half of tools-disk.sh, run in a throwaway clone of metal.test with the tools disk as
# its second disk (/dev/disk/by-id/virtio-disk1). Formats it if blank and fills /opt/perf-compute
# with everything perf-compute.sh runs, built so a FRESH clone of the image can run it with nothing
# installed on its root:
#   lib/       liblimina_metal.so (relinked from the host tree's static library at every run) and the
#              runtime libraries the builds need that the image lacks (OpenBLAS, LAPACK, gfortran)
#   prefix/    MLX_METAL_LIMINA: metal-cpp, the stand-in headers, mlx.metallib and the JIT sources
#   venv/      MLX's Python package, built from the pinned fork, and mlx-lm
#   llama.cpp/ llama.cpp at LLAMA_CPP_REV with its Vulkan backend: llama-bench
#   bench/     metal-bench and its metallib, mlx_bench.py
#   models/    the models, as the host downloaded them
#   MANIFEST   what all of it was built from
# The host has already unpacked its staging tree into /opt/perf-compute/stage.
# Usage (by tools-disk.sh): guest-provision.sh <llama.cpp rev> <mlx-lm version>
set -euo pipefail
LLAMA_REV=$1
MLX_LM=$2
T=/opt/perf-compute
S=$T/stage
export PATH=/usr/bin:/usr/sbin

log() { echo "== $(date +%H:%M:%S) $*"; }

# Build tools go on this throwaway root; the dnf cache on the tools disk, the root has little room.
# What the image has before we install anything decides which libraries the disk must carry; a
# second pass over the same root would see its own installs as the image's.
[ -f $T/base-packages.txt ] || rpm -qa --qf '%{NAME}\n' | sort > $T/base-packages.txt
log "dnf"
sudo dnf -y -q --setopt=cachedir=$T/dnf-cache --setopt=keepcache=0 install \
  clang cmake ninja-build git openblas-devel lapack-devel python3-devel \
  glslc spirv-headers-devel vulkan-headers vulkan-loader-devel
sudo rm -rf $T/dnf-cache

log "prefix and liblimina_metal.so"
rm -rf $T/prefix && cp -a $S/prefix $T/prefix
mkdir -p $T/lib
gcc -shared -o $T/lib/liblimina_metal.so -Wl,--whole-archive $S/liblimina_metal.a \
  -Wl,--no-whole-archive -lpthread -ldl -lm -lgcc_s
# MLX looks for limina-metal in the prefix; one copy, so a relink reaches every consumer.
ln -sf $T/lib/liblimina_metal.so $T/prefix/lib/liblimina_metal.so

log "MLX python package"
rm -rf $T/mlx && mkdir -p $T/mlx && tar -C $T/mlx -xf $S/mlx-src.tar
python3 -m venv --clear $T/venv
. $T/venv/bin/activate
pip install -q setuptools typing_extensions numpy
# Fedora's GNUInstallDirs would put libmlx.so in lib64/, where the extension's $ORIGIN/lib runpath
# does not look. No .git in the source, so DEV_RELEASE keeps the commit out of the version.
(cd $T/mlx/mlx && DEV_RELEASE=1 CMAKE_BUILD_PARALLEL_LEVEL=$(nproc) \
  CMAKE_ARGS="-DMLX_METAL_LIMINA=$T/prefix -DCMAKE_INSTALL_LIBDIR=lib -DCMAKE_C_COMPILER=clang -DCMAKE_CXX_COMPILER=clang++" \
  pip install --no-build-isolation . > $T/mlx-build.log 2>&1) || { tail -n 40 $T/mlx-build.log; exit 1; }
# mlx-lm without its mlx requirement, which pip would take from PyPI; then its own requirements.
pip install -q --no-deps "mlx-lm==$MLX_LM"
# shellcheck disable=SC2046 # one word per requirement
pip install -q $(python3 -c 'import importlib.metadata as m; print(" ".join(r.split(";")[0].replace(" ", "") for r in m.requires("mlx-lm") if "extra ==" not in r and not r.startswith("mlx")))')
deactivate

log "llama.cpp $LLAMA_REV"
rm -rf $T/llama.cpp && mkdir -p $T/llama.cpp && tar -C $T/llama.cpp -xf $S/llama.cpp-src.tar
cmake -S $T/llama.cpp/src -B $T/llama.cpp/build -G Ninja -DCMAKE_BUILD_TYPE=Release \
  -DGGML_VULKAN=ON -DGGML_BLAS=OFF -DLLAMA_CURL=OFF -DLLAMA_BUILD_TESTS=OFF -DLLAMA_BUILD_EXAMPLES=OFF \
  -DLLAMA_BUILD_SERVER=OFF -DLLAMA_BUILD_TOOLS=ON -DGGML_NATIVE=ON > $T/llama-configure.log 2>&1 ||
  { tail -n 40 $T/llama-configure.log; exit 1; }
ninja -C $T/llama.cpp/build llama-bench > $T/llama-build.log 2>&1 || { tail -n 40 $T/llama-build.log; exit 1; }

log "metal-bench"
rm -rf $T/bench && mkdir -p $T/bench
cp $S/metal-bench.cpp $S/metal-bench.metallib $S/mlx_bench.py $T/bench/
clang++ -std=c++17 -fblocks -O2 -Wno-ignored-attributes -I $T/prefix/include -I $T/prefix/metal-cpp \
  $T/bench/metal-bench.cpp -L $T/lib -llimina_metal -Wl,-rpath,$T/lib -o $T/bench/metal-bench

log "runtime libraries the image lacks"
for f in $T/venv/lib/python3*/site-packages/mlx/lib/libmlx.so $T/llama.cpp/build/bin/llama-bench \
         $T/llama.cpp/build/bin/*.so $T/bench/metal-bench; do
  LD_LIBRARY_PATH=$T/lib ldd "$f" | awk '/=> \//{print $3}'
done | sort -u | while read -r lib; do
  case "$lib" in $T/*) continue ;; esac
  pkg=$(rpm -qf --qf '%{NAME}\n' "$lib" 2>/dev/null) || continue
  grep -qx "$pkg" $T/base-packages.txt || { cp -L "$lib" $T/lib/; echo "  $lib ($pkg)"; }
done

log "models"
rm -rf $T/models && mv $S/models $T/models

{
  echo "built $(date -u +%Y-%m-%dT%H:%M:%SZ) on $(uname -r), $(python3 --version)"
  echo "mlx $(cat $T/mlx/mlx/LIMINA_SOURCE_COMMIT)"
  echo "llama.cpp $LLAMA_REV, $(glslc --version | head -n 1)"
  echo "clang $(clang --version | head -n 1)"
  echo "venv:"; $T/venv/bin/pip freeze | sed 's/^/  /'
  echo "models:"; (cd $T/models && find . -name '*.gguf' -o -name '*.safetensors' | sort | xargs sha256sum | sed 's/^/  /')
} > $T/MANIFEST
rm -rf $S
log "done"
cat $T/MANIFEST
