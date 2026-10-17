#!/bin/bash
# The host half of MLX on Linux: everything that needs Apple's toolchain, into out/.
#   out/prefix/   the MLX_METAL_LIMINA prefix: lib/liblimina_metal.a (the guest makes the .so),
#                 include/ (Darwin stand-ins), metal-cpp/, and mlx/ = mlx.metallib, the JIT
#                 sources and config.cmake, all from the MLX fork's HEAD
#   out/mlx-src.tar  that HEAD, with LIMINA_SOURCE_COMMIT naming it
# Usage: host.sh [mlx-tree]   (default: the main checkout's third_party/mlx, branch limina)
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
shim=$here/../metal-cpp-shim
mlx=${1:-$(git -C "$here" rev-parse --path-format=absolute --git-common-dir)/../third_party/mlx}
mlx=$(cd "$mlx" && pwd)
out=$here/out
prefix=$out/prefix
jit=OFF

if [ -n "$(git -C "$mlx" status --porcelain --untracked-files=no)" ]; then
  echo "$mlx has uncommitted changes; the artifacts must name a commit" >&2
  exit 1
fi
commit=$(git -C "$mlx" rev-parse HEAD)
mkdir -p "$prefix/lib" "$prefix/include" "$prefix/mlx/jit"

# limina-metal, its stand-in headers and metal-cpp, from the shim spike's build.
"$shim/build.sh"
cp "$shim/out/liblimina_metal.a" "$prefix/lib/"
cp -R "$shim/include/." "$prefix/include/"
cp -R "$shim/out/metal-cpp/." "$prefix/metal-cpp/"

# mlx.metallib, built by MLX's own CMake for this host.
cmake -S "$mlx" -B "$out/host-build" -DCMAKE_BUILD_TYPE=Release -DMLX_METAL_JIT=$jit \
  -DMLX_BUILD_TESTS=OFF -DMLX_BUILD_EXAMPLES=OFF -DMLX_BUILD_BENCHMARKS=OFF \
  -DMLX_BUILD_PYTHON_BINDINGS=OFF > "$out/host-configure.log"
cmake --build "$out/host-build" --target mlx-metallib -j "$(sysctl -n hw.ncpu)" > "$out/host-metallib.log"
cp "$out/host-build/mlx/backend/metal/kernels/mlx.metallib" "$prefix/mlx/"

# Every JIT source MLX can ask for, by MLX's own script; a build uses the ones its configuration
# names.
srcs=$(python3 -I -c 'import re, sys
s = open(sys.argv[1]).read()
print(*sorted({m for m in re.findall(r"make_jit_source\(\s*([A-Za-z0-9_/]+)", s)} - {"SRC_FILE"}))' \
  "$mlx/mlx/backend/metal/CMakeLists.txt")
for s in $srcs; do
  bash "$mlx/mlx/backend/metal/make_compiled_preamble.sh" "$prefix/mlx/jit" cc "$mlx" "$s"
done

cat > "$prefix/mlx/config.cmake" <<EOF
# Written by spikes/mlx-linux/host.sh: what these artifacts were built from.
set(MLX_LIMINA_HOST_COMMIT $commit)
set(MLX_LIMINA_HOST_JIT $jit)
EOF

git -C "$mlx" archive --format=tar --prefix=mlx/ -o "$out/mlx-src.tar" HEAD
echo "$commit" > "$out/LIMINA_SOURCE_COMMIT"
tar -C "$out" --append -f "$out/mlx-src.tar" -s ',^LIMINA_SOURCE_COMMIT,mlx/LIMINA_SOURCE_COMMIT,' LIMINA_SOURCE_COMMIT
echo "MLX $commit: $(ls "$prefix/mlx/jit" | wc -l | tr -d ' ') JIT sources, $(du -h "$prefix/mlx/mlx.metallib" | cut -f1) metallib"
