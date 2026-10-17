#!/bin/bash
# The guest half of MLX on Linux: copies host.sh's out/ into a guest and builds MLX there against
# limina-metal, with its tests and examples. Steps, all by default:
#   copy    out/prefix and the MLX source into ~/mlxl
#   deps    cmake, clang, git, OpenBLAS and LAPACK (MLX's CPU backend)
#   so      liblimina_metal.so from the static library, so a new shim needs no MLX relink
#   build   MLX, shared, Release, with MLX_METAL_LIMINA=~/mlxl/prefix
# and, when named: check (test/check.cpp, the GPU against MLX's CPU backend), or MLX's C++ examples: tutorial, linear_regression, logistic_regression.
# Usage: guest.sh <ssh-port> [step...]. The test images' credentials are in docs/images.md.
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
port=$1
shift
steps=${*:-copy deps so build}
user=${GUEST_USER:-claude}
pass=${GUEST_PASS:-claudiusrobotus}
O=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR)
g() { gtimeout --kill-after=10 "$1" sshpass -p "$pass" ssh -p "$port" "${O[@]}" "$user@127.0.0.1" "$2"; }

for step in $steps; do
  echo "== $step"
  case $step in
  copy)
    g 30 'mkdir -p ~/mlxl' || exit 1
    tar --no-mac-metadata --no-xattrs -C "$here/out" -cf - prefix mlx-src.tar |
      gtimeout --kill-after=10 600 sshpass -p "$pass" ssh -p "$port" "${O[@]}" "$user@127.0.0.1" \
        'cd ~/mlxl && tar -xf - && tar -xf mlx-src.tar' || exit 1
    ;;
  deps)
    g 1800 "echo '$pass' | sudo -S dnf -y -q install cmake clang git openblas-devel lapack-devel" || exit 1
    ;;
  so)
    g 300 'cd ~/mlxl/prefix/lib && clang -shared -o liblimina_metal.so \
      -Wl,--whole-archive liblimina_metal.a -Wl,--no-whole-archive -lpthread -ldl -lm -lgcc_s' || exit 1
    ;;
  build)
    g 600 'cd ~/mlxl && cmake -S mlx -B build -DCMAKE_BUILD_TYPE=Release \
      -DCMAKE_C_COMPILER=clang -DCMAKE_CXX_COMPILER=clang++ -DBUILD_SHARED_LIBS=ON \
      -DMLX_METAL_LIMINA=$HOME/mlxl/prefix -DMLX_BUILD_TESTS=ON -DMLX_BUILD_EXAMPLES=ON \
      -DMLX_BUILD_BENCHMARKS=OFF -DMLX_BUILD_PYTHON_BINDINGS=OFF > configure.log 2>&1; rc=$?; \
      tail -20 configure.log; exit $rc' || exit 1
    g 7200 'cd ~/mlxl && cmake --build build -j "$(nproc)" -- -k > build.log 2>&1; rc=$?; \
      grep -E "error|Error" build.log | head -40; tail -5 build.log; exit $rc' || exit 1
    ;;
  check)
    tar --no-mac-metadata --no-xattrs -C "$here" -cf - test |
      gtimeout --kill-after=10 60 sshpass -p "$pass" ssh -p "$port" "${O[@]}" "$user@127.0.0.1" \
        'cd ~/mlxl && tar -xf -' || exit 1
    g 600 'cd ~/mlxl && clang++ -std=c++20 -O1 -g -I mlx -I build test/check.cpp -L build -lmlx \
      -o check > check-build.log 2>&1 || { head -c 4000 check-build.log; exit 1; }
      LD_LIBRARY_PATH=$HOME/mlxl/prefix/lib:$HOME/mlxl/build ./check 2>&1; echo exit=$?' || exit 1
    ;;
  tutorial | linear_regression | logistic_regression)
    g 600 "cd ~/mlxl && LD_LIBRARY_PATH=\$HOME/mlxl/prefix/lib:\$HOME/mlxl/build build/examples/cpp/$step 2>&1; echo exit=\$?"
    ;;
  *)
    echo "unknown step $step" >&2
    exit 2
    ;;
  esac
done
