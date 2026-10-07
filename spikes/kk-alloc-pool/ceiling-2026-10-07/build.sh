#!/usr/bin/env bash
# Private zink-on-KK build of the limina-kk-hardening worktree, asserts ON.
set -euo pipefail
SRC=/Volumes/mesa-cs/mesa-hardening
BUILD=/Volumes/mesa-cs/build-kk-hardening
PREFIX=/Volumes/mesa-cs/zink-kk-prefix-hardening
source "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)/third_party/venv-mesa/bin/activate"
export PATH="$(brew --prefix bison)/bin:$(brew --prefix llvm)/bin:$PATH"
export PKG_CONFIG_PATH="$(brew --prefix)/lib/pkgconfig:$(brew --prefix)/share/pkgconfig:$(brew --prefix expat)/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
if [ ! -d "$BUILD" ]; then
  meson setup "$BUILD" "$SRC" -Dplatforms=macos -Dvulkan-drivers=kosmickrisp -Dgallium-drivers=zink \
    -Dopengl=true -Dgles2=enabled -Degl=enabled -Dglx=disabled -Dglvnd=disabled -Dshared-llvm=enabled \
    -Dzstd=disabled -Dprefer_static=true -Dbuildtype=debugoptimized \
    -Dmoltenvk-dir="$(brew --prefix molten-vk)" -Dprefix="$PREFIX" -Degl-native-platform=surfaceless \
    -Db_ndebug=false
fi
ninja -j2 -C "$BUILD"
meson install -C "$BUILD" >/dev/null
echo "installed to $PREFIX"
