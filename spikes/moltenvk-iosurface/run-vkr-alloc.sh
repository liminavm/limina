#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build + run vkr_alloc_test.m against the REAL vkr_mtl_iosurface_alloc object from the
# built virglrenderer (proves limina tier-2 allocator B host-side). Build virglrenderer
# first: scripts/build-virglrenderer.sh
set -euo pipefail
cd "$(dirname "$0")"
ROOT="$(cd ../.. && pwd)"

OBJ="$ROOT/third_party/virglrenderer/build/src/libvirgl.a.p/venus_vkr_metal_helpers.m.o"
[ -f "$OBJ" ] || { echo "missing $OBJ — run scripts/build-virglrenderer.sh first"; exit 1; }

clang -g -O0 -fno-objc-arc \
  -framework Foundation -framework Metal -framework IOSurface \
  vkr_alloc_test.m "$OBJ" -o vkr_alloc_test
exec ./vkr_alloc_test
