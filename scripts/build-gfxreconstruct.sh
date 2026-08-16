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

# Pin for reproducibility; override with GFXR_TAG=<tag-or-sha>. "latest" resolves against
# the repo's release TAGS at clone time — which is why the default is a commit, not
# "latest": the newest tag (v1.0.4) is far behind main and its replay summary line still
# says "Replay FPS", while `crates/limina-test/tests/venus_replay.rs` greps for the
# current "Measured FPS". A "latest" build therefore replays perfectly and fails the test
# on wording alone (2026-08-16 — it cost a rebuild to notice the tag was the old one).
# 765c3d6 is the commit the enhanced goldens' /opt/gfxreconstruct was built from; move
# this pin and the test's grep together, never one alone.
GFXR_TAG="${GFXR_TAG:-765c3d6}"
GFXR_GIT="${GFXR_GIT:-https://github.com/LunarG/gfxreconstruct.git}"

OUT="$REPO/target/test-guest/gfxreconstruct"
OUTROOT="$REPO/target/test-guest"
mkdir -p "$OUT"

VOL="limina-gfxr-build"
container volume inspect "$VOL" >/dev/null 2>&1 || container volume create "$VOL" >/dev/null

scripts/build-image.sh   # ensure the unified limina-build image (cmake/xcb/X11/wayland deps baked)
container run --rm \
  --cpus 8 --memory 12g \
  -v "$OUT:/out" \
  -v "$OUTROOT:/outroot" \
  -v "$VOL:/build" \
  limina-build:fc43 bash -euxo pipefail -c '
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
    # -j4, rather than the default width: at 765c3d6 the generated Vulkan/OpenXR encoder
    # translation units each want >2 GB in cc1plus, and eight of them in parallel OOM the
    # container ("c++: fatal error: Killed signal terminated program cc1plus", 2026-08-16).
    # The memory bump above is not enough on its own — the limit is per-compile, so the
    # width is what has to come down.
    ninja -C build -j4
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
