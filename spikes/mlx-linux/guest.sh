#!/bin/bash
# The guest half of MLX on Linux: copies host.sh's out/ into a guest and builds MLX there against
# limina-metal, with its tests and examples. Steps, all by default:
#   copy    out/prefix and the MLX source into ~/mlxl
#   deps    cmake, clang, git, OpenBLAS and LAPACK (MLX's CPU backend)
#   so      liblimina_metal.so from the static library, so a new shim needs no MLX relink
#   build   MLX, shared, Release, with MLX_METAL_LIMINA=~/mlxl/prefix
# and, when named: check (test/check.cpp, the GPU against MLX's CPU backend), python (MLX's Python
# package, in ~/mlxl/venv), lm (mlx-lm in that venv), model (test/lm_check.py on the GPU and the
# CPU, compared by test/lm_compare.py; MODEL picks the model), omlx (omlx in that venv), serve
# (omlx serving SERVE_MODEL, checked by test/serve_check.py), or MLX's C++ examples: tutorial, linear_regression, logistic_regression.
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
  python)
    # MLX's own setup.py, which runs its CMake with CMAKE_ARGS appended. No .git in the copied
    # source, so DEV_RELEASE skips the commit hash in the version. Fedora's GNUInstallDirs would put
    # libmlx.so in lib64/, where the extension's $ORIGIN/lib runpath does not look.
    g 1800 "echo '$pass' | sudo -S dnf -y -q install python3-devel python3-pip" || exit 1
    g 3600 'cd ~/mlxl && { [ -d venv ] || python3 -m venv venv; } && . venv/bin/activate &&
      pip install -q setuptools typing_extensions numpy && cd mlx &&
      DEV_RELEASE=1 CMAKE_ARGS="-DMLX_METAL_LIMINA=$HOME/mlxl/prefix -DCMAKE_INSTALL_LIBDIR=lib -DCMAKE_C_COMPILER=clang -DCMAKE_CXX_COMPILER=clang++" \
      pip install --no-build-isolation . > ~/mlxl/python-build.log 2>&1; rc=$?
      tail -5 ~/mlxl/python-build.log; exit $rc' || exit 1
    g 120 'cd ~/mlxl && . venv/bin/activate && cd /tmp &&
      LD_LIBRARY_PATH=$HOME/mlxl/prefix/lib python3 -c "import mlx.core as mx; a = mx.arange(8.0); print(mx.default_device(), (a * 2 + 1).tolist())" 2>&1 | grep -v "os error"' || exit 1
    ;;
  lm)
    # mlx-lm without its mlx requirement, which pip would take from PyPI; then its own deps.
    g 1800 'cd ~/mlxl && . venv/bin/activate && pip install -q --no-deps mlx-lm &&
      pip install -q $(python3 -c "import importlib.metadata as m; print(\" \".join(r.split(\";\")[0].replace(\" \", \"\") for r in m.requires(\"mlx-lm\") if \"extra ==\" not in r and not r.startswith(\"mlx\")))") &&
      pip show mlx-lm | grep -i "^version"' || exit 1
    ;;
  omlx)
    # omlx at a pinned commit, over the fork's MLX: its plain requirements as pip resolves them,
    # the git-pinned MLX packages and omlx itself with --no-deps. omlx declares Python < 3.14
    # and macOS; the guest has 3.14.
    rev=${OMLX_REV:-8a9da1b44f608c7231e9b7c341e120d8cfc2d16a}
    tar --no-mac-metadata --no-xattrs -C "$here" -cf - test |
      gtimeout --kill-after=10 60 sshpass -p "$pass" ssh -p "$port" "${O[@]}" "$user@127.0.0.1" \
        'cd ~/mlxl && tar -xf -' || exit 1
    g 3600 "cd ~/mlxl && . venv/bin/activate &&
      { [ -d omlx ] || git clone -q https://github.com/jundot/omlx.git; } &&
      git -C omlx fetch -q origin $rev && git -C omlx checkout -q $rev &&
      python3 test/omlx_reqs.py omlx/pyproject.toml omlx-plain.txt omlx-pinned.txt &&
      pip install -q -r omlx-plain.txt > omlx-install.log 2>&1 &&
      pip install -q --no-deps -r omlx-pinned.txt >> omlx-install.log 2>&1 &&
      pip install -q --no-deps --ignore-requires-python ./omlx >> omlx-install.log 2>&1;
      rc=\$?; tail -5 omlx-install.log; [ \$rc -eq 0 ] || exit \$rc
      LD_LIBRARY_PATH=\$HOME/mlxl/prefix/lib python3 -c 'import mlx.core as mx, mlx_lm, omlx; print(\"mlx\", mx.__version__, \"omlx\", omlx.__version__)'" || exit 1
    ;;
  serve)
    # omlx serving SERVE_MODEL (a 7B 4-bit by default) from ~/models, driven by test/serve_check.py
    # over HTTP, then stopped. Its settings and SSD cache go in ~/.omlx. omlx renames its process
    # omlx-server, so it is stopped by the pid it wrote, not by a pattern.
    model=${SERVE_MODEL:-mlx-community/Qwen2.5-7B-Instruct-4bit}
    tar --no-mac-metadata --no-xattrs -C "$here" -cf - test |
      gtimeout --kill-after=10 60 sshpass -p "$pass" ssh -p "$port" "${O[@]}" "$user@127.0.0.1" \
        'cd ~/mlxl && tar -xf -' || exit 1
    g 3600 "cd ~/mlxl && . venv/bin/activate && export LD_LIBRARY_PATH=\$HOME/mlxl/prefix/lib &&
      { [ -f ~/models/$model/config.json ] || hf download $model --local-dir ~/models/$model > dl.log 2>&1; } &&
      { ! curl -s -m 2 http://127.0.0.1:8001/ > /dev/null || { echo 'port 8001 is taken'; exit 1; }; } &&
      { setsid sh -c 'echo \$\$ > omlx.pid; exec omlx serve --model-dir ~/models --port 8001 --headless' \\
          > omlx-serve.log 2>&1 < /dev/null & } &&
      for i in \$(seq 1 90); do curl -s -m 2 http://127.0.0.1:8001/v1/models > /dev/null && break; sleep 2; done &&
      timeout 1800 python3 test/serve_check.py http://127.0.0.1:8001 ${model##*/}; rc=\$?
      kill \$(cat omlx.pid) || rc=1; exit \$rc" || exit 1
    ;;
  model)
    # A small 4-bit model through mlx-lm, on the GPU and on MLX's CPU backend, compared.
    model=${MODEL:-mlx-community/Qwen2.5-0.5B-Instruct-4bit}
    tar --no-mac-metadata --no-xattrs -C "$here" -cf - test |
      gtimeout --kill-after=10 60 sshpass -p "$pass" ssh -p "$port" "${O[@]}" "$user@127.0.0.1" \
        'cd ~/mlxl && tar -xf -' || exit 1
    g 3600 "cd ~/mlxl && . venv/bin/activate && export LD_LIBRARY_PATH=\$HOME/mlxl/prefix/lib &&
      python3 test/lm_check.py $model gpu lm-gpu 2>&1 | grep -v 'os error' &&
      python3 test/lm_check.py $model cpu lm-cpu 2>&1 | grep -v 'os error' &&
      python3 test/lm_compare.py lm-gpu lm-cpu" || exit 1
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
