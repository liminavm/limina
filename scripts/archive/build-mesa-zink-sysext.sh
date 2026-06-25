#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build the limina enhanced-tier patched Mesa (zink megadriver + venus + the whole GL/EGL/GBM
# closure) and assemble it into a **systemd-sysext** — the PRODUCTIZED guest-mesa delivery
# (supersedes the /opt/mesa-zink alternate-prefix + LD_LIBRARY_PATH crutch, which is
# antithetical to sysext: a sysext overlays the REAL /usr tree, it does not add a prefix).
#
# WHAT WE SHIP — the FULL mesa /usr install, not just the megadriver:
#   libgallium-<VER>.so          the gallium megadriver (our zink guard series 0001-0006)
#   dri/*_dri.so                 loader symlinks -> our megadriver
#   libEGL_mesa.so.0 / libGLX_mesa.so.0   the glvnd VENDOR libs (-Dglvnd=true)
#   libgbm.so.1, libgallium, libvulkan_virtio.so + ICD, glvnd vendor JSONs, ...
# WHY the whole footprint (decision 2026-06-24): stock F43 is mesa **25.3.6**; we build
# **26.2.0-devel** (dev-enh's exact /opt/mesa-zink commit 3515c52 — the proven zink). Mesa gives NO
# cross-version guarantee on its internal DRI-driver <-> glvnd-vendor ABI, so shadowing only
# the megadriver and leaving stock's 25.3.6 libEGL_mesa/libGLX_mesa in place is an untested
# ABI blend. Instead the sysext carries our ENTIRE mesa install and shadows stock's mesa
# wholesale — every GL/EGL/Vulkan file the guest loads is ours and mutually consistent
# (exactly what the dev-enh golden image did with its self-contained /opt/mesa-zink prefix).
#
# ONE build, BOTH patch series: 0001-0006 (zink: MR!37115 nullDescriptor + guards) AND
# 0009-0010 (venus present fix). 0009-0010 target 26.1.0 and may need a 26.2 rebase (applied
# tolerantly below); the zink EGL fix does not depend on them.
#
# FEDORA_REL MUST match the guest's Fedora release: the build links the guest's libLLVM AND
# libdisplay-info sonames. Building in fedora:44 for an fedora:43 guest links
# libdisplay-info.so.3 vs the guest's .so.2 -> the Vulkan loader skips the venus ICD ->
# VK_ERROR_INCOMPATIBLE_DRIVER -> zink falls back -> no accelerated desktop.
#
# Output (gitignored):
#   target/test-guest/mesa-sysext/                              the sysext tree (rsync into
#     usr/{lib64,share,...}                                     /var/lib/extensions/<name>/)
#     usr/lib/extension-release.d/extension-release.limina-mesa
# Driver SELECTION (GALLIUM_DRIVER=zink) is a separate concern — the first-boot installer
# drops a static /etc/environment.d file, NOT this build.
set -euxo pipefail
cd "$(dirname "$0")/.."
REPO="$PWD"

# zink MUST be mesa 26.2 (the PROVEN dev-enh /opt/mesa-zink commit). The seated venus desktop's
# EGL/gbm render-device only initializes on 26.2 zink; the 26.1.0 we shipped before left mutter's
# render-device at EGL_NO_DISPLAY -> "GPU /dev/dri/card0 ... not supported by EGL" -> crash-loop.
MESA_VER="${MESA_VER:-26.2.0-devel}"
MESA_COMMIT="${MESA_COMMIT:-3515c52e8cf31549b6068ef43c23c89830b6db46}"
MESA_GIT="${MESA_GIT:-https://gitlab.freedesktop.org/mesa/mesa.git}"
FEDORA_REL="${FEDORA_REL:-43}"
EXT_NAME="limina-mesa"

OUTROOT="$REPO/target/test-guest"
EXT="$OUTROOT/mesa-sysext"
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
    dnf -y install git meson ninja-build gcc gcc-c++ python3-mako bison flex \
      glslang-devel zstd libglvnd-devel >/dev/null
    dnf -y builddep mesa >/dev/null

    cd /build
    [ -d mesa/.git ] || git clone "'"$MESA_GIT"'" mesa
    cd mesa
    git config --global --add safe.directory /build/mesa
    rm -f .git/shallow.lock .git/index.lock .git/*.lock 2>/dev/null || true
    git fetch --depth=1 origin "'"$MESA_COMMIT"'" || git fetch origin
    git checkout -f "'"$MESA_COMMIT"'"
    git clean -fdx -e build-sysext 2>/dev/null || true
    echo "=== mesa HEAD ==="; git log --oneline -1; cat VERSION

    # Patch series: 0001 (MR!37115 nullDescriptor — LOAD-BEARING for zink-on-KK) + zink guards
    # (0002-0006) target 26.2 and must apply; 0009-0010 (venus present fix) were authored over
    # 26.1.0 and may need a 26.2 rebase — apply tolerantly and report so a venus-side miss does
    # not block the zink EGL fix (the EGL_NO_DISPLAY bug is purely zink-side).
    for p in $(ls /patches/*.diff 2>/dev/null | sort); do
      echo "=== applying $(basename "$p") ==="
      if git apply --verbose "$p"; then echo "OK: $(basename "$p")";
      elif patch -p1 -F5 < "$p" >/dev/null 2>&1; then echo "OK(fuzz): $(basename "$p")";
      else echo "SKIP (does not apply on 26.2 — needs rebase): $(basename "$p")"; fi
    done

    # Full GL closure: glvnd vendor libs (so the guest glvnd loads OUR libEGL_mesa/libGLX_mesa),
    # EGL/GLES/GBM, the gallium megadriver (zink + virgl baseline + sw fallbacks), and venus.
    rm -rf build-sysext
    # DESKTOP GL ENABLED (-Dglx=dri, no -Dopengl=false) — matches the PROVEN dev-enh
    # /opt/mesa-zink meson config. The earlier GLES-only experiment did NOT fix mutter''s
    # render-device EGL probe: it fails at EGL_NO_DISPLAY, a step BEFORE any GL/GLES context
    # bind, so removing desktop GL was never relevant and is restored to match the known-good
    # build. glvnd=true is REQUIRED: this sysext shadows Fedora''s glvnd-based /usr, so we must
    # ship the glvnd VENDOR libs (libEGL_mesa.so.0 + egl_vendor.d JSON) the system libEGL.so.1
    # dispatches to — a non-glvnd build would emit a colliding classic libEGL.so.1 soname.
    meson setup build-sysext \
      --prefix=/usr --libdir=lib64 \
      -Dgallium-drivers=zink,virgl,llvmpipe,softpipe \
      -Dvulkan-drivers=virtio \
      -Dplatforms=x11,wayland \
      -Dglvnd=true -Dglx=dri -Degl=enabled -Dgbm=enabled -Dgles2=enabled \
      -Dllvm=enabled -Dbuildtype=release
    ninja -C build-sysext
    rm -rf /tmp/stage
    DESTDIR=/tmp/stage ninja -C build-sysext install

    echo "=== mesa install footprint (what the sysext ships) ==="
    ( cd /tmp/stage && find usr -type f -o -type l ) | sort
    GALLIUM=$(ls /tmp/stage/usr/lib64/libgallium-*.so | head -1)
    VENUS=/tmp/stage/usr/lib64/libvulkan_virtio.so
    echo "--- sonames the guest must satisfy (matched by building in fedora:'"$FEDORA_REL"') ---"
    ( ldd "$GALLIUM" "$VENUS" 2>/dev/null || true ) | grep -iE "LLVM|display-info|=> not found" | sort -u | head

    # Assemble the sysext tree on the host bind mount: ship the WHOLE mesa /usr install so the
    # entire GL/EGL/Vulkan stack the guest loads is ours and self-consistent (see header).
    EXT=/outroot/mesa-sysext
    rm -rf "$EXT"; mkdir -p "$EXT/usr/lib/extension-release.d"
    cp -a /tmp/stage/usr/. "$EXT/usr/"
    # extension-release gates loading; must match the guest os-release ID/VERSION_ID.
    cat > "$EXT/usr/lib/extension-release.d/extension-release.'"$EXT_NAME"'" <<EOF
ID=fedora
VERSION_ID='"$FEDORA_REL"'
EOF
    echo "=== sysext assembled ==="; du -sh "$EXT"; ls "$EXT/usr/lib64" | grep -iE "gallium|EGL|GLX|gbm|vulkan_virtio" | head
  '
echo "==> done: $EXT (full mesa-$MESA_VER closure, shadows stock mesa). Deliver via systemd-sysext."
