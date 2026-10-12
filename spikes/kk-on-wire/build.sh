#!/bin/bash
# Builds kk-probe for an aarch64 Fedora 44 guest in the limina-build:fc44 container.
# Usage: spikes/kk-on-wire/build.sh  ->  spikes/kk-on-wire/out/kk-probe
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$here/out"
container run --rm -v "$here:/src" limina-build:fc44 bash -c '
set -e
dnf install -y -q libavcodec-free-devel libavformat-free-devel libavutil-free-devel >/dev/null
cd /src
for s in fill.comp tri.vert tri.frag; do
  glslangValidator -V --vn "$(echo $s | tr . _)" -o "out/$s.h" "$s" >/dev/null
done
gcc -O2 -Wall -Werror -o out/bufcaps bufcaps.c -lvulkan
gcc -O2 -Wall -Werror -Iout -I/usr/include/libdrm -o out/kk-probe probe.c -lvulkan -lgbm -lEGL -lGLESv2 -lva -lva-drm -ldrm $(pkg-config --cflags --libs libavcodec libavformat libavutil)
'
