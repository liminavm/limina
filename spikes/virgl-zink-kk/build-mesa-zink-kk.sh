#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Spike build: reconfigure the KK Mesa tree to ALSO build zink (gallium GL→Vulkan) + EGL
# (surfaceless headless platform) + desktop GL, natively on macOS arm64 — so we can test a
# host GL context on zink-on-KosmicKrisp WITHOUT ANGLE. See README.md.
#
# Source: /Volumes/mesa-cs/mesa (case-sensitive sparseimage — APFS can't check out mesa).
# Build:  /Volumes/mesa-cs/build-zink-kk   (kept off the build-kk dir so KK stays intact)
# Prefix: /Volumes/mesa-cs/zink-kk-prefix
set -euxo pipefail

SRC="${MESA_SRC:-/Volumes/mesa-cs/mesa}"
BUILD="${MESA_BUILD:-/Volumes/mesa-cs/build-zink-kk}"
PREFIX="${MESA_PREFIX:-/Volumes/mesa-cs/zink-kk-prefix}"

[ -d "$SRC/.git" ] || { echo "KK mesa source missing at $SRC (mount mesa-cs.sparseimage)" >&2; exit 1; }

# mako (Mesa codegen) from the repo venv; meson/ninja/glslang from homebrew.
REPO="$(cd "$(dirname "$0")/../.." && pwd)"
# shellcheck disable=SC1091
source "$REPO/third_party/venv-mesa/bin/activate"

# Toolchain quirks the host KK build also needs (else meson fails to even configure):
#  - LLVM 22 is keg-only on Homebrew → its llvm-config isn't on PATH. KosmicKrisp REQUIRES it
#    (with_kosmickrisp_vk ∈ with_driver_using_cl → CLC → LLVM), so prepend it.
#  - expat is keg-only → add its pkgconfig (the EGL/dri driconf parser needs it).
#  - Apple's /usr/bin/bison is 2.3 (2008); Mesa's GLSL glcpp grammar needs bison > 2.3.
#    KK (opengl=false) never built glcpp so it didn't hit this; zink/GL does. `brew install bison`.
export PATH="$(brew --prefix bison)/bin:$(brew --prefix llvm)/bin:$PATH"
export PKG_CONFIG_PATH="$(brew --prefix)/lib/pkgconfig:$(brew --prefix)/share/pkgconfig:$(brew --prefix expat)/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"

# Mirror KK's macos config, then add the GL stack:
#  -Dgallium-drivers=zink     zink GL→Vulkan
#  -Dvulkan-drivers=kosmickrisp  keep KK so we can point zink at it (VK_DRIVER_FILES at runtime)
#  -Dopengl=true + -Dgles2=enabled   desktop GL + GLES (kopper pbuffer wants gles)
#  -Degl=enabled              build EGL → pulls in surfaceless + device platforms (always-on)
#  -Dglx=disabled -Dglvnd=disabled   skip the X11/CGL apple-GLX path; we want headless EGL only
#  -Dplatforms=macos          keep the macos Vulkan WSI (egl-native default = macos; the probe
#                             uses EGL_PLATFORM_SURFACELESS_MESA explicitly regardless)
MESON_ARGS=(
  -Dplatforms=macos
  -Dvulkan-drivers=kosmickrisp
  -Dgallium-drivers=zink
  -Dopengl=true
  -Dgles2=enabled
  -Degl=enabled
  # macos is a valid WINDOWING platform but NOT a valid egl-native-platform (no _EGL_PLATFORM_MACOS
  # enum). With egl=enabled Mesa appends 'surfaceless' to the platform list, but auto picks
  # platforms[0]='macos' → undefined _EGL_PLATFORM_MACOS → compile error. Pin the EGL default to
  # surfaceless (headless, exactly what vrend wants); we also call eglGetPlatformDisplay(SURFACELESS).
  -Degl-native-platform=surfaceless
  -Dglx=disabled
  -Dglvnd=disabled
  -Dshared-llvm=enabled
  -Dzstd=disabled
  -Dprefer_static=true
  # debugoptimized (default): MESA_DEBUG=0 so mesa_logd is compiled out (no "MESA: debug: ..."
  # spam in the release .app), -O2, asserts still ON. Override with BUILDTYPE=debug for active
  # KK/zink debugging (adds mesa_logd + -O0). See docs/drivers/kosmickrisp.rst.
  -Dbuildtype="${BUILDTYPE:-debugoptimized}"
  # zink's darwin path needs the MoltenVK headers (IOSurface/CAMetalLayer types + kopper
  # winsys) to COMPILE; at runtime zink talks to whatever ICD VK_DRIVER_FILES names (KK).
  -Dmoltenvk-dir="$(brew --prefix molten-vk)"
  --prefix "$PREFIX"
)

if [ -d "$BUILD" ]; then
  meson setup --reconfigure "$BUILD" "$SRC" "${MESON_ARGS[@]}"
else
  meson setup "$BUILD" "$SRC" "${MESON_ARGS[@]}"
fi
ninja -C "$BUILD"
meson install -C "$BUILD"

echo "==> installed zink-on-KK Mesa to $PREFIX"
find "$PREFIX" -name 'libEGL*' -o -name 'libgallium*' -o -name '*zink*' 2>/dev/null | head
