#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build an instrumented KosmicKrisp ICD beside the shared one, never over it.
#
# scripts/build-host-mesa.sh knows only $MOUNT/build-kk, which every boot script on this host
# opens; rebuilding it with the tally in would put the instrument (and its cost) under other
# sessions' VMs. This builds the KK half from a separate Mesa worktree into a separate build
# dir with the same flags, and prints the ICD json to point VK_ICD_FILENAMES at.
#
# Usage: build-kk-tally.sh [mesa-tree] [build-dir]
#   defaults: /Volumes/mesa-cs/mesa-mnc  /Volumes/mesa-cs/build-kk-tally
# The tree must already be instrumented (gen_tally.py). libclc comes from the main checkout's
# pinned copy, which scripts/build-host-mesa.sh fetched; override with LIBCLC_PC_DIR.
set -euo pipefail
cd "$(dirname "$0")/../.."
ROOT="$(pwd)"

SRC="${1:-/Volumes/mesa-cs/mesa-mnc}"
BUILD="${2:-/Volumes/mesa-cs/build-kk-tally}"
[ -e "$SRC/src/kosmickrisp/bridge/mtl_tally.h" ] \
  || { echo "$SRC is not instrumented: run gen_tally.py on it first" >&2; exit 1; }

export PATH="$(brew --prefix bison)/bin:$(brew --prefix llvm)/bin:$PATH"
LIBCLC_PC_DIR="${LIBCLC_PC_DIR:-$(git -C "$ROOT" worktree list --porcelain | awk 'NR==1{print $2}')/third_party/libclc/share/pkgconfig}"
export PKG_CONFIG_PATH="$LIBCLC_PC_DIR:$(brew --prefix)/lib/pkgconfig:$(brew --prefix)/share/pkgconfig"
pkg-config --exists libclc || { echo "no libclc.pc in $LIBCLC_PC_DIR" >&2; exit 1; }

# Same reason as build-host-mesa.sh: Homebrew's clang resolves ld through PATH.
APPLE_LD="$(xcrun -f ld 2>/dev/null || echo /usr/bin/ld)"
if [ "$(command -v ld || true)" != "$APPLE_LD" ]; then
  mkdir -p "$ROOT/target/host-mesa/bin"
  ln -sf "$APPLE_LD" "$ROOT/target/host-mesa/bin/ld"
  export PATH="$ROOT/target/host-mesa/bin:$PATH"
fi

# shellcheck source=scripts/ensure-venv-mesa.sh
. "$ROOT/scripts/ensure-venv-mesa.sh"

# Identical to build-host-mesa.sh's `kk` half, so the tally is the only delta.
ARGS=(-Dplatforms=macos -Dvulkan-drivers=kosmickrisp -Dzstd=disabled -Dprefer_static=true
      -Dbuildtype=debugoptimized -Db_ndebug=true -Dgallium-drivers= -Dopengl=false)
if [ -d "$BUILD" ]; then
  meson setup --reconfigure "$BUILD" "$SRC" "${ARGS[@]}"
else
  meson setup "$BUILD" "$SRC" "${ARGS[@]}"
fi
ninja -C "$BUILD"

echo "==> mesa source: $SRC @ $(git -C "$SRC" rev-parse HEAD) + tally"
icd=("$BUILD"/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.*.json)
# grep -c, not -q: -q exits at the first match, nm takes SIGPIPE, and pipefail reads that as absent.
nm "$BUILD/src/kosmickrisp/vulkan/libvulkan_kosmickrisp.dylib" | grep -c mtl_tally_enter >/dev/null \
  || { echo "the built ICD has no mtl_tally_enter: the instrument is not in it" >&2; exit 1; }
echo "==> VK_ICD_FILENAMES=${icd[0]}"
