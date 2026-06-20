#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build libepoxy WITH EGL support on macOS — the one prerequisite for virglrenderer's vrend
# (GL renderer) on Darwin. Homebrew's epoxy is CGL-only (epoxy_has_egl=0), so vrend's
# `-Dplatforms=egl` won't configure against it. epoxy auto-disables EGL on darwin; -Degl=yes
# forces it. EGL/GLES headers come from our zink-on-KK Mesa prefix; at runtime epoxy dispatches
# via eglGetProcAddress into whatever libEGL/libGLESv2 is loaded (our Mesa). See README.md.
#
# Source: third_party/libepoxy (gitignored). Output: third_party/epoxy-egl-prefix.
set -euxo pipefail
cd "$(dirname "$0")/../.."
REPO="$PWD"

SRC="$REPO/third_party/libepoxy"
BUILD="$SRC/build-egl"
PREFIX="$REPO/third_party/epoxy-egl-prefix"
MESA_PREFIX="${MESA_PREFIX:-/Volumes/mesa-cs/zink-kk-prefix}"

[ -f "$SRC/meson.build" ] || { echo "libepoxy source missing at $SRC" >&2; exit 1; }

source "$REPO/third_party/venv-mesa/bin/activate" 2>/dev/null || true
# Our Mesa first (egl.pc/glesv2.pc), then homebrew.
export PKG_CONFIG_PATH="$MESA_PREFIX/lib/pkgconfig:$(brew --prefix)/lib/pkgconfig:$(brew --prefix)/share/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"

MESON_ARGS=(
  -Degl=yes          # force EGL on (auto disables it on darwin)
  -Dglx=no           # no X11/GLX
  -Dx11=false
  -Dtests=false
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

echo "==> epoxy(+EGL) installed to $PREFIX"
pkg-config --variable=epoxy_has_egl --define-variable=prefix="$PREFIX" "$PREFIX/lib/pkgconfig/epoxy.pc" 2>/dev/null \
  || grep -i epoxy_has_egl "$PREFIX/lib/pkgconfig/epoxy.pc" 2>/dev/null
