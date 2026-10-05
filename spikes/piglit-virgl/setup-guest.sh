#!/bin/bash
# setup-guest.sh — build piglit IN TREE in a Fedora guest (run inside the guest as a sudoer).
# In tree, because an out-of-tree build leaves piglit unable to find its .shader_test files and
# compiler tests (82 tests "fail" that way without ever reaching the driver).
set -eu
REV=${PIGLIT_REV:-c3aa5b9}
sudo dnf -y -q install git cmake ninja-build gcc gcc-c++ waffle-devel mesa-libGL-devel \
  mesa-libEGL-devel mesa-libgbm-devel libdrm-devel libX11-devel libXrender-devel libxcb-devel \
  libxkbcommon-devel wayland-devel wayland-protocols-devel libpng-devel python3-mako python3-numpy python3-lxml \
  python3-pyyaml
[ -d ~/piglit ] || git clone -q https://gitlab.freedesktop.org/mesa/piglit.git ~/piglit
cd ~/piglit
git checkout -q "$REV"
cmake -G Ninja -DCMAKE_BUILD_TYPE=Release -DPIGLIT_BUILD_GLES2_TESTS=ON \
  -DPIGLIT_BUILD_GLES3_TESTS=ON -DPIGLIT_BUILD_CL_TESTS=OFF -DPIGLIT_BUILD_VK_TESTS=OFF . > cmake.log 2>&1
ninja > ninja.log 2>&1
echo "piglit $(git rev-parse --short HEAD) built in tree"
