#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build the limina enhanced-tier Mesa (patched zink) reproducibly in the Apple `container`
# Linux build env — replacing the ad-hoc in-guest ~/mesa build (task #26). Mirrors
# scripts/build-test-kernel.sh: clone + build happen INSIDE the container (ext4, case-
# sensitive — the host APFS is case-insensitive and can't even check out mesa cleanly),
# and only the finished install tree is bind-mounted out.
#
# What it builds: the patched **zink** gallium driver (GL→Vulkan) carrying MR !37115
# (nullDescriptor emulation for MoltenVK). Venus (libvulkan_virtio) is the guest's STOCK
# Mesa Vulkan driver — we do NOT build it here (-Dvulkan-drivers= empty). zink is pointed
# at venus at runtime via VK_DRIVER_FILES (see scripts/venus-gl-test.sh).
#
# Output: target/test-guest/mesa-zink/  (an install prefix = the /opt/mesa-zink we ship
# into the guest) + mesa-zink.tar.zst for delivery. Both gitignored.
#
# IMPORTANT: this carries our patch series `patches/mesa/*.diff` (clean — NO LIMINA-DIAG
# debug hacks; strip any before re-exporting). Base is pinned to MESA_COMMIT below.
#
# STATUS (2026-06-07): authored from the proven in-guest recipe (~/build-mesa-zink.sh);
# not yet run end-to-end in-container — validate on first run (see task #26).
set -euxo pipefail
cd "$(dirname "$0")/.."
REPO="$PWD"

# Pin to the mesa main commit the in-guest /opt/mesa-zink was built from (2026-06-07).
MESA_COMMIT="${MESA_COMMIT:-3515c52e8cf31549b6068ef43c23c89830b6db46}"
# Anubis blocks gitlab.freedesktop.org's *web UI*, NOT the git protocol — `git clone` over
# https works there (that's how the in-guest /opt/mesa-zink was built). Override with MESA_GIT.
MESA_GIT="${MESA_GIT:-https://gitlab.freedesktop.org/mesa/mesa.git}"

OUT="$REPO/target/test-guest/mesa-zink"
OUTROOT="$REPO/target/test-guest"
mkdir -p "$OUT"

# Persistent ext4 build volume (source clone + ccache + ninja objects survive re-runs),
# same pattern as the kernel's limina-kbuild-* volume.
VOL="limina-mesa-build"
container volume inspect "$VOL" >/dev/null 2>&1 || container volume create "$VOL" >/dev/null

container run --rm \
  --cpus 8 --memory 8g \
  -v "$REPO/patches/mesa:/patches:ro" \
  -v "$OUT:/out" \
  -v "$OUTROOT:/outroot" \
  -v "$VOL:/build" \
  fedora:43 bash -euxo pipefail -c '
    dnf -y install git meson ninja-build gcc gcc-c++ python3-mako bison flex glslang-devel zstd >/dev/null
    dnf -y builddep mesa >/dev/null

    cd /build
    if [ ! -d mesa/.git ]; then
      git clone "'"$MESA_GIT"'" mesa || git clone https://github.com/Igalia/mesa.git mesa
    fi
    cd mesa
    git fetch --depth=1 origin "'"$MESA_COMMIT"'" || git fetch origin
    git checkout -f "'"$MESA_COMMIT"'"
    git clean -fdx -e build-zink 2>/dev/null || true

    # Apply our clean patch series (order by filename).
    for p in $(ls /patches/*.diff 2>/dev/null | sort); do
      echo "=== applying $p ==="
      git apply --verbose "$p" || patch -p1 -F3 < "$p"
    done

    rm -rf build-zink
    meson setup build-zink \
      --prefix=/opt/mesa-zink \
      -Dgallium-drivers=zink,llvmpipe,softpipe \
      -Dvulkan-drivers= \
      -Dplatforms=x11,wayland \
      -Dglx=dri -Degl=enabled -Dgbm=enabled -Dgles2=enabled \
      -Dllvm=enabled -Dbuildtype=release
    ninja -C build-zink
    DESTDIR=/tmp/stage ninja -C build-zink install

    # Export the install tree (becomes /opt/mesa-zink in the guest). Tar FIRST (the
    # canonical delivery artifact), then mirror into /out — cp -a cannot set times on
    # the bind-mount root, so tolerate that (files copy fine; only the root mtime fails).
    # NOTE the tar must go through a bind mount (/outroot): "/out/.." is the container
    # filesystem, not the host — an earlier version silently dropped the tar there.
    tar -C /tmp/stage/opt -cf - mesa-zink | zstd -19 -o /outroot/mesa-zink.tar.zst -f
    rm -rf /out/*
    cp -a /tmp/stage/opt/mesa-zink/. /out/ 2>/dev/null || cp -rp /tmp/stage/opt/mesa-zink/. /out/ 2>/dev/null || true
    ls -la /out/lib64/dri/ | grep -iE "zink|dril" || true
  '
echo "==> done: $OUT (+ target/test-guest/mesa-zink.tar.zst). Deliver to the guest /opt/mesa-zink."
