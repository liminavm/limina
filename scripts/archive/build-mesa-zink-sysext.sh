#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build the limina enhanced-tier patched **zink megadriver + venus** as
# **version-matched in-place** Mesa libraries and assemble them into a **systemd-sysext**
# — the PRODUCTIZED guest-mesa delivery (supersedes the /opt/mesa-zink alternate-prefix +
# env crutch in build-mesa-zink.sh, which is antithetical to sysext: a sysext overlays the
# REAL /usr tree, it does not add an LD_LIBRARY_PATH prefix).
#
# Ships BOTH halves of the guest GL stack:
#   - libgallium-<VER>.so   — the gallium megadriver carrying our zink guard series
#   - libvulkan_virtio.so   — venus carrying patch 0008 (dma_buf on the opaque-fd KK
#                             renderer); the load-bearing fix a stock guest needs to not
#                             SIGSEGV gnome-shell. (Earlier this script was zink-only.)
#
# FEDORA_REL MUST match the guest's Fedora release: the build links the guest's libLLVM
# AND libdisplay-info sonames. Building venus in fedora:43 for an fedora:44 guest links
# libdisplay-info.so.2 vs the guest's .so.3 → the Vulkan loader skips the ICD →
# VK_ERROR_INCOMPATIBLE_DRIVER → zink falls back → no accelerated desktop.
#
# Model: Fedora ships the whole gallium driver set inside ONE megadriver,
# /usr/lib64/libgallium-<VER>.so (zink/virgl/llvmpipe are symlinks
# dri/*_dri.so -> libdril_dri.so -> that megadriver). We build the SAME mesa version
# (so same soname, ABI-compatible with the stock libGL/libEGL/libdril) carrying our
# clean zink guard series (patches/mesa/*.diff), and the sysext SHADOWS that one file.
# limina owns the guest mesa version, so the soname stays locked to what we ship — the
# usual "distro bumps the version and the shadow silently stops applying" fragility
# does not bite us.
#
# Output:
#   target/test-guest/mesa-zink-sysext/                 (the sysext tree, ready to rsync
#     usr/lib64/libgallium-<VER>.so                      into /var/lib/extensions/<name>/)
#     usr/lib/extension-release.d/extension-release.limina-mesa-zink
#   target/test-guest/libgallium-<VER>.so               (the bare shadow file too)
# Both gitignored. Driver SELECTION (GALLIUM_DRIVER=zink) is a separate concern — a
# static /etc/environment.d file, NOT this build.
set -euxo pipefail
cd "$(dirname "$0")/.."
REPO="$PWD"

# Match the guest's stock mesa EXACTLY (mesa-dri-drivers-26.0.3-4.fc44 -> upstream 26.0.3).
MESA_VER="${MESA_VER:-26.0.3}"
MESA_TAG="${MESA_TAG:-mesa-$MESA_VER}"
MESA_GIT="${MESA_GIT:-https://gitlab.freedesktop.org/mesa/mesa.git}"
# Build env must match the guest so libgallium links the guest's libLLVM soname.
FEDORA_REL="${FEDORA_REL:-44}"
EXT_NAME="limina-mesa-zink"

OUTROOT="$REPO/target/test-guest"
EXT="$OUTROOT/mesa-zink-sysext"
mkdir -p "$OUTROOT"
rm -rf "$EXT"

VOL="limina-mesa-build"
container volume inspect "$VOL" >/dev/null 2>&1 || container volume create "$VOL" >/dev/null

container run --rm \
  --cpus 8 --memory 8g \
  -v "$REPO/patches/mesa:/patches:ro" \
  -v "$OUTROOT:/outroot" \
  -v "$VOL:/build" \
  "fedora:$FEDORA_REL" bash -euxo pipefail -c '
    dnf -y install git meson ninja-build gcc gcc-c++ python3-mako bison flex glslang-devel zstd >/dev/null
    dnf -y builddep mesa >/dev/null

    cd /build
    if [ ! -d mesa/.git ]; then
      git clone "'"$MESA_GIT"'" mesa
    fi
    cd mesa
    git fetch --tags --depth=1 origin "'"$MESA_TAG"'"
    git checkout -f "'"$MESA_TAG"'"
    git clean -fdx -e build-sysext 2>/dev/null || true

    # Apply our clean zink guard series; report each (some may already be upstream in
    # this release -> apply fails -> skip loudly, do NOT silently drop).
    for p in $(ls /patches/*.diff 2>/dev/null | sort); do
      echo "=== applying $(basename "$p") ==="
      if git apply --verbose "$p"; then echo "OK: $p";
      elif patch -p1 -F5 --dry-run < "$p" >/dev/null 2>&1 && patch -p1 -F5 < "$p"; then echo "OK(fuzz): $p";
      else echo "SKIP/FAILED (likely already upstream or needs rebase): $p"; fi
    done

    # Same gallium driver set the guest actually uses (zink + virgl baseline + sw
    # fallbacks); other distro drivers (panfrost/freedreno/...) are unused in the VM,
    # so omitting them keeps the megadriver a valid drop-in for what we load.
    # Build the gallium megadriver (zink + virgl baseline + sw fallbacks) AND venus
    # (libvulkan_virtio) in ONE mesa build, so both carry our patch series and link the
    # SAME (guest-matched) libLLVM / libdisplay-info sonames. venus is the load-bearing
    # piece: patch 0008 makes it advertise dma_buf on the opaque-fd KK renderer; without
    # it a stock guest gbm-create_dumbs a NULL bo and gnome-shell SIGSEGVs.
    rm -rf build-sysext
    meson setup build-sysext \
      --prefix=/usr --libdir=lib64 \
      -Dgallium-drivers=zink,virgl,llvmpipe,softpipe \
      -Dvulkan-drivers=virtio \
      -Dplatforms=x11,wayland \
      -Dglx=dri -Degl=enabled -Dgbm=enabled -Dgles2=enabled \
      -Dllvm=enabled -Dbuildtype=release
    ninja -C build-sysext
    DESTDIR=/tmp/stage ninja -C build-sysext install

    GALLIUM=$(ls /tmp/stage/usr/lib64/libgallium-*.so | head -1)
    VENUS=/tmp/stage/usr/lib64/libvulkan_virtio.so
    VENUS_ICD=$(ls /tmp/stage/usr/share/vulkan/icd.d/*virtio*.json | head -1)
    echo "built megadriver: $GALLIUM ; venus: $VENUS"
    ls -la "$GALLIUM" "$VENUS"
    echo "--- shared-lib deps that must exist on the guest (matched by building in fedora:'"$FEDORA_REL"') ---"
    ( ldd "$GALLIUM" "$VENUS" 2>/dev/null || true ) | grep -iE "LLVM|display-info|=> not found" | sort -u | head

    # Assemble the sysext tree on the host bind mount. The sysext SHADOWS the stock
    # version-matched files in /usr — libgallium-<VER>.so AND libvulkan_virtio.so — plus
    # the venus ICD (its api_version may differ from stock). limina owns the guest mesa
    # version, so the sonames stay locked to what we ship.
    EXT=/outroot/mesa-zink-sysext
    rm -rf "$EXT"
    mkdir -p "$EXT/usr/lib64" "$EXT/usr/share/vulkan/icd.d" "$EXT/usr/lib/extension-release.d"
    cp -a "$GALLIUM" "$EXT/usr/lib64/"
    cp -a "$VENUS"   "$EXT/usr/lib64/"
    cp -a "$VENUS_ICD" "$EXT/usr/share/vulkan/icd.d/"
    cp -a "$GALLIUM" "/outroot/$(basename "$GALLIUM")"
    cp -a "$VENUS"   "/outroot/$(basename "$VENUS")"
    cat > "$EXT/usr/lib/extension-release.d/extension-release.'"$EXT_NAME"'" <<EOF
ID=fedora
VERSION_ID='"$FEDORA_REL"'
EOF
    echo "=== sysext tree ==="; find "$EXT" -type f -o -type l | sed "s#$EXT/##"
  '
echo "==> done: $EXT (shadows libgallium-$MESA_VER.so + libvulkan_virtio.so + venus ICD). Deliver via systemd-sysext."
