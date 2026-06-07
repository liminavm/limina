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

if echo "$link" | grep -q 'third_party/virgl-prefix'; then
    echo "check-virgl-link: OK — $WORKER links our virgl-prefix virglrenderer"
    exit 0
fi

cat >&2 <<EOF
check-virgl-link: WRONG VIRGLRENDERER LINK — venus will silently degrade to software-2D!
  $WORKER links:
$link
  Expected: .../third_party/virgl-prefix/lib/libvirglrenderer.*.dylib
  Fix: rebuild the worker so build.rs/pkg-config resolves our prefix, e.g.
    PKG_CONFIG_PATH="\$PWD/third_party/virgl-prefix/lib/pkgconfig:\$(brew --prefix)/opt/molten-vk/lib/pkgconfig:\$(brew --prefix)/lib/pkgconfig:\$(brew --prefix)/share/pkgconfig" \\
      cargo build -p limina-vmm && crates/limina-vmm/sign.sh debug
  (build.rs now prepends the prefix automatically, so a clean rebuild should fix it.)
  If third_party/virgl-prefix is missing, run scripts/build-virglrenderer.sh first.
EOF
exit 1
