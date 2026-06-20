#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build virglrenderer with the EGL platform (vrend GL renderer) ENABLED, in addition to venus —
# the eventual coexist device wants both. Mirrors scripts/build-virglrenderer.sh but flips
# -Dplatforms=[] → -Dplatforms=egl and puts our epoxy-with-EGL first on PKG_CONFIG_PATH.
# On Darwin virglrenderer enables EGL WITHOUT gbm (meson.build: `with_host_darwin → have_egl=true`)
# and uses the virgl_egl_init(display_id, surfaceless, gles) path → surfaceless EGL → zink → KK.
#
# Installs to a SEPARATE prefix (third_party/virgl-gl-prefix) so the working venus-only
# third_party/virgl-prefix the worker links today is untouched until this is proven. See README.md.
set -euxo pipefail
cd "$(dirname "$0")/../.."
REPO="$PWD"

SRC="${VIRGL_SRC:-$REPO/third_party/virglrenderer}"
BUILD="$SRC/build-gl"
PREFIX="${VIRGL_GL_PREFIX:-$REPO/third_party/virgl-gl-prefix}"
MESA_PREFIX="${MESA_PREFIX:-/Volumes/mesa-cs/zink-kk-prefix}"
EPOXY_PREFIX="$REPO/third_party/epoxy-egl-prefix"

[ -f "$SRC/meson.build" ] || { echo "virglrenderer source missing at $SRC" >&2; exit 1; }
grep -qi epoxy_has_egl=1 "$EPOXY_PREFIX/lib/pkgconfig/epoxy.pc" 2>/dev/null \
  || { echo "epoxy-with-EGL missing — run build-epoxy-egl.sh first" >&2; exit 1; }

source "$REPO/third_party/venv-mesa/bin/activate" 2>/dev/null || true
# Order matters: our epoxy-egl FIRST (epoxy_has_egl=1), then zink-kk Mesa (EGL/GLES headers),
# then molten-vk (venus ICD discovery) + homebrew (vulkan-loader, etc.).
export PKG_CONFIG_PATH="$EPOXY_PREFIX/lib/pkgconfig:$MESA_PREFIX/lib/pkgconfig:$(brew --prefix)/opt/molten-vk/lib/pkgconfig:$(brew --prefix)/lib/pkgconfig:$(brew --prefix)/share/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"

MESON_ARGS=(
  -Dvenus=true
  -Dvulkan-dload=false
  -Drender-server-mode=thread
  -Drender-server-worker=thread
  -Dplatforms=egl
  --prefix "$PREFIX"
  --buildtype release
)

if [ -d "$BUILD" ]; then
  meson setup --reconfigure "$BUILD" "$SRC" "${MESON_ARGS[@]}"
else
  meson setup "$BUILD" "$SRC" "${MESON_ARGS[@]}"
fi
ninja -C "$BUILD"
meson install -C "$BUILD"

echo "==> virglrenderer (venus + vrend/EGL) installed to $PREFIX"
otool -L "$PREFIX"/lib/libvirglrenderer*.dylib 2>/dev/null | grep -iE 'epoxy|EGL|vulkan' || true
