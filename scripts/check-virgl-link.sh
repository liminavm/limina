#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Guard against the costly silent trap (see CLAUDE.md / memory limina-virgl-link-trap):
# the worker MUST link our patched third_party/virgl-prefix virglrenderer, NOT Homebrew's.
# If it links Homebrew's, virgl_renderer_init returns -1 and the GPU silently degrades to
# software-2D (venus never enumerates) with no obvious error. Call this after building/
# signing the worker and before booting anything that needs venus.
#
# Usage: scripts/check-virgl-link.sh [path-to-limina-vmm]   (default: target/debug/limina-vmm)
set -euo pipefail
WORKER="${1:-target/debug/limina-vmm}"
# What the worker is SUPPOSED to link -- the same variable build.rs and build-app.sh use, so
# the guard checks the prefix that was asked for rather than a hardcoded one. Hardcoding it
# would fail an intentional virglrs build while passing a stale prefix, which inverts the check.
PREFIX="${VIRGL_PREFIX:-$PWD/third_party/virgl-prefix}"

if [[ ! -x "$WORKER" ]]; then
    echo "check-virgl-link: worker not found at $WORKER (build it first)" >&2
    exit 1
fi

link="$(otool -L "$WORKER" 2>/dev/null | grep -i virglrenderer || true)"
if [[ -z "$link" ]]; then
    echo "check-virgl-link: $WORKER does not link virglrenderer at all?!" >&2
    echo "$link" >&2
    exit 1
fi

if echo "$link" | grep -qF "$PREFIX/lib/"; then
    echo "check-virgl-link: OK — $WORKER links $PREFIX"
    exit 0
fi

cat >&2 <<EOF
check-virgl-link: WRONG VIRGLRENDERER LINK — venus will silently degrade to software-2D!
  $WORKER links:
$link
  Expected: $PREFIX/lib/libvirglrenderer.*.dylib
  Fix: rebuild the worker so build.rs/pkg-config resolves our prefix, e.g.
    PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig:\$(brew --prefix)/opt/molten-vk/lib/pkgconfig:\$(brew --prefix)/lib/pkgconfig:\$(brew --prefix)/share/pkgconfig" \\
      cargo build -p limina-vmm && crates/limina-vmm/sign.sh debug
  (build.rs now prepends the prefix automatically, so a clean rebuild should fix it.)
  If $PREFIX is missing, run scripts/build-virglrenderer.sh (or virglrs/install.sh) first.
EOF
exit 1
