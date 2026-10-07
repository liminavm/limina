#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Private zink-on-KK build of a Mesa tree, so the shared builds on /Volumes/mesa-cs stay untouched.
#   build.sh <src> <build-dir> <prefix>
set -euo pipefail
SRC="$1" BUILD="$2" PREFIX="$3"
ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
. "$ROOT/scripts/ensure-venv-mesa.sh"
export PATH="$(brew --prefix bison)/bin:$(brew --prefix llvm)/bin:$PATH"
export PKG_CONFIG_PATH="$(brew --prefix)/lib/pkgconfig:$(brew --prefix)/share/pkgconfig:$(brew --prefix expat)/lib/pkgconfig:$ROOT/third_party/libclc/share/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
if [ ! -d "$BUILD" ]; then
  meson setup "$BUILD" "$SRC" -Dplatforms=macos -Dvulkan-drivers=kosmickrisp -Dgallium-drivers=zink \
    -Dopengl=true -Dgles2=enabled -Degl=enabled -Dglx=disabled -Dglvnd=disabled -Dshared-llvm=enabled \
    -Dzstd=disabled -Dprefer_static=true -Dbuildtype=debugoptimized -Db_ndebug=true \
    -Dmoltenvk-dir="$(brew --prefix molten-vk)" -Dprefix="$PREFIX" -Degl-native-platform=surfaceless
fi
ninja -j6 -C "$BUILD"
meson install -C "$BUILD" >/dev/null
echo "installed to $PREFIX"
