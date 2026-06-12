#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build LunarG gfxreconstruct (Vulkan capture layer + gfxrecon-replay) for the guest in
# the Apple `container` Linux build env — it is NOT packaged in Fedora 43. Same pattern
# as scripts/build-mesa-zink.sh: clone + build inside the container, ship the install
# prefix out as a tar for /opt/gfxreconstruct in the guest.
#
# Used by the trace-replay phase-2 tests (native Vulkan capture/replay — venus without
# zink; see docs/roadmap.md "Trace-replay rendering tests" and memory
# `limina-trace-replay-plan`). In the guest:
#   capture: VK_LAYER_PATH=/opt/gfxreconstruct/share/vulkan/explicit_layer.d \
#            LD_LIBRARY_PATH=/opt/gfxreconstruct/lib64 \
#            VK_INSTANCE_LAYERS=VK_LAYER_LUNARG_gfxreconstruct vkcube ...
#   replay:  /opt/gfxreconstruct/bin/gfxrecon-replay [--screenshots ...] file.gfxr
#
# Output: target/test-guest/gfxreconstruct/ + gfxreconstruct.tar.zst (both gitignored).
set -euxo pipefail
cd "$(dirname "$0")/.."
REPO="$PWD"

# Pin for reproducibility; override with GFXR_TAG=<tag>. Resolved against the repo's
# release tags at clone time when set to "latest".
GFXR_TAG="${GFXR_TAG:-latest}"
GFXR_GIT="${GFXR_GIT:-https://github.com/LunarG/gfxreconstruct.git}"

OUT="$REPO/target/test-guest/gfxreconstruct"
OUTROOT="$REPO/target/test-guest"
mkdir -p "$OUT"

VOL="limina-gfxr-build"
container volume inspect "$VOL" >/dev/null 2>&1 || container volume create "$VOL" >/dev/null

container run --rm \
  --cpus 8 --memory 8g \
  -v "$OUT:/out" \
  -v "$OUTROOT:/outroot" \
  -v "$VOL:/build" \
  fedora:43 bash -euxo pipefail -c '
    dnf -y install git cmake ninja-build gcc gcc-c++ python3 zstd \
      lz4-devel libzstd-devel zlib-ng-compat-devel \
      libxcb-devel libX11-devel libXrandr-devel wayland-devel libwayland-client \
      xcb-util-keysyms-devel xcb-util-wm-devel >/dev/null

    cd /build
    if [ ! -d gfxreconstruct/.git ]; then
      git clone "'"$GFXR_GIT"'" gfxreconstruct
    fi
    cd gfxreconstruct
    git fetch --tags origin
    TAG="'"$GFXR_TAG"'"
    if [ "$TAG" = latest ]; then
      TAG=$(git tag -l "v[0-9]*" --sort=-version:refname | head -1)
    fi
    echo "=== building gfxreconstruct $TAG ==="
    git checkout -f "$TAG"
    git submodule update --init --recursive

    rm -rf build
    # gcc 15 deprecates std::wstring_convert; the project builds -Werror — downgrade
    # just that class so a pinned v1.0.4 keeps building on newer toolchains.
    cmake -S . -B build -G Ninja \
      -DCMAKE_BUILD_TYPE=Release \
      -DCMAKE_INSTALL_PREFIX=/opt/gfxreconstruct \
      -DCMAKE_INSTALL_LIBDIR=lib64 \
      -DCMAKE_CXX_FLAGS="-Wno-error=deprecated-declarations"
    ninja -C build
    DESTDIR=/tmp/stage ninja -C build install

    # The layer manifest must point at the layer .so by absolute path (we install
    # outside the loader default dirs).
    MANIFEST=$(ls /tmp/stage/opt/gfxreconstruct/share/vulkan/explicit_layer.d/*.json)
    sed -i "s|\"library_path\": \"[^\"]*\"|\"library_path\": \"/opt/gfxreconstruct/lib64/libVkLayer_gfxreconstruct.so\"|" "$MANIFEST"
    cat "$MANIFEST"

    tar -C /tmp/stage/opt -cf - gfxreconstruct | zstd -19 -o /outroot/gfxreconstruct.tar.zst -f
    rm -rf /out/*
    cp -a /tmp/stage/opt/gfxreconstruct/. /out/ 2>/dev/null || cp -rp /tmp/stage/opt/gfxreconstruct/. /out/ 2>/dev/null || true
    ls /out/bin /out/lib64
  '
echo "==> done: $OUT (+ target/test-guest/gfxreconstruct.tar.zst). Deliver to the guest /opt/gfxreconstruct."
