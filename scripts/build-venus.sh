#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build ONLY venus (libvulkan_virtio.so) at a chosen mesa version + our venus patches,
# in a Fedora `container` matched to the guest. The enhanced tier's WORKING venus is the
# dev-enh golden image's, whose base was pinned (blob fingerprint, 2026-06-24) to the
# mesa-26.1.0 release; the present fix was NOT a version regression but a LOST limina patch
# (0009+0010) that only ever lived in dev-enh's ~/mesa-venus tree — see docs/dev-enh-recipe.md.
# We pin to mesa-26.1.0 because that's dev-enh's exact base (the patches are pure-limina diffs
# against it); zink stays at whatever the guest ships. dev-enh ran zink 26.2 + venus 26.1 mixed,
# so this is faithful. Porting 0009/0010 forward to 26.2 is optional future work.
#
# Applies ONLY the venus patches (0009 WSI present fix + 0010 image/physdev native modifier)
# — NOT the zink guard series, which is irrelevant to a venus-only build and may not apply
# on an older tag.
#
#   MESA_TAG=mesa-26.1.0 FEDORA_REL=43 scripts/build-venus.sh
# Output: target/test-guest/venus-<tag>/libvulkan_virtio.so (+ the ICD json). gitignored.
set -euxo pipefail
cd "$(dirname "$0")/.."
REPO="$PWD"

MESA_TAG="${MESA_TAG:-mesa-26.1.0}"
MESA_GIT="${MESA_GIT:-https://gitlab.freedesktop.org/mesa/mesa.git}"
# Build distro MUST match the guest: venus links the guest's libdisplay-info / libLLVM
# sonames (build-mesa-zink-sysext.sh learned this the hard way). Clone guest = Fedora 43.
FEDORA_REL="${FEDORA_REL:-43}"

OUTROOT="$REPO/target/test-guest"
OUT="$OUTROOT/venus-$MESA_TAG"
mkdir -p "$OUT"

VOL="limina-mesa-build"
container volume inspect "$VOL" >/dev/null 2>&1 || container volume create "$VOL" >/dev/null

container run --rm \
  --cpus 8 --memory 8g \
  -v "$REPO/patches/mesa:/patches:ro" \
  -v "$OUT:/out" \
  -v "$VOL:/build" \
  "fedora:$FEDORA_REL" bash -euxo pipefail -c '
    dnf -y install git meson ninja-build gcc gcc-c++ python3-mako bison flex glslang-devel >/dev/null
    dnf -y builddep mesa >/dev/null

    cd /build
    [ -d mesa/.git ] || git clone "'"$MESA_GIT"'" mesa
    cd mesa
    git config --global --add safe.directory /build/mesa
    rm -f .git/shallow.lock .git/index.lock .git/*.lock 2>/dev/null || true
    git fetch --tags --depth=1 origin "'"$MESA_TAG"'"
    git checkout -f "'"$MESA_TAG"'"
    git clean -fdx -e build-venus 2>/dev/null || true
    echo "=== mesa HEAD ==="; git log --oneline -1; cat VERSION

    # venus patches only (present-region + dma_buf/modifier). Report each.
    # The venus present fix, reverse-engineered from the dev-enh golden image (the edits
    # had only ever lived in its ~/mesa-venus tree). Together they reproduce the working
    # accelerated-tier venus on a clean mesa-26.1.0:
    #   0009 = WSI: treat-invalid-modifier-as-linear (wsi_common*) + DRM_FORMAT_MODIFIER
    #          ->OPTIMAL swapchain xlate + present-region deep-copy (supersedes old 0005).
    #   0010 = vn_physical_device native LINEAR-modifier reporting + dma_buf-on-opaque-fd
    #          (supersedes old 0008) + vn_image modifier plane/tiling handling.
    for p in /patches/0009-venus-wsi-present-fix.diff \
             /patches/0010-venus-image-physdev-native-modifier.diff; do
      echo "=== applying $(basename "$p") ==="
      if git apply --verbose "$p"; then echo "OK: $p";
      elif patch -p1 -F5 --dry-run < "$p" >/dev/null 2>&1 && patch -p1 -F5 < "$p"; then echo "OK(fuzz): $p";
      else echo "FAILED: $p — will need the version-robust venus-dmabuf-patch.py"; exit 3; fi
    done

    # dev-enh venus build options exactly: -Dvulkan-drivers=virtio -Dplatforms=wayland
    # -Dllvm=disabled, default (debug) buildtype -> ~27M, matches the known-good binary.
    rm -rf build-venus
    meson setup build-venus \
      --prefix=/usr --libdir=lib64 \
      -Dgallium-drivers=zink \
      -Dvulkan-drivers=virtio \
      -Dplatforms=wayland \
      -Dglx=disabled -Degl=enabled -Dgbm=enabled -Dgles2=enabled \
      -Dllvm=disabled
    ninja -C build-venus src/virtio/vulkan/libvulkan_virtio.so
    DESTDIR=/tmp/stage ninja -C build-venus install 2>/dev/null || true

    VENUS=$(find build-venus -name libvulkan_virtio.so | head -1)
    ICD=$(find build-venus /tmp/stage -name "*virtio*icd*.json" 2>/dev/null | head -1)
    ls -la "$VENUS"
    cp -a "$VENUS" /out/libvulkan_virtio.so
    [ -n "$ICD" ] && cp -a "$ICD" /out/ || true
    echo "--- sonames the guest must satisfy (matched by fedora:'"$FEDORA_REL"') ---"
    ( ldd "$VENUS" 2>/dev/null || true ) | grep -iE "LLVM|display-info|=> not found" | sort -u
    echo "=== built venus -> /out/libvulkan_virtio.so ==="
  '
echo "==> done: $OUT/libvulkan_virtio.so"
