#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Assemble the limina-virtio-gpu DKMS source tree: the in-tree virtio-gpu DRM driver
# sources for a given stable-kernel series + limina's host-visible window alignment
# patch (guest/virtio-gpu-dkms/0001, see the README there), packaged so
# `dkms add/build/install` on a STOCK Fedora
# guest shadows the in-tree module (depmod prefers extra/) — venus on stock 4 KiB
# guests, with normal kernel updates still flowing (AUTOINSTALL rebuilds per kernel;
# a failed build degrades gracefully to the in-tree driver).
#
# Usage: scripts/build-virtio-gpu-dkms.sh [KTAG]
#   KTAG: stable kernel tag matching the target guest's kernel series
#         (default v6.19.10 — the F44 stock kernel; see docs/images.md).
# Output: target/virtio-gpu-dkms/limina-virtio-gpu-<version>/ (the DKMS tree)
#         target/virtio-gpu-dkms/limina-virtio-gpu-<version>.tar.gz
#
# In-guest install (needs: dnf install dkms kernel-devel gcc make):
#   tar xzf limina-virtio-gpu-<v>.tar.gz -C /usr/src
#   dkms add     limina-virtio-gpu/<v>
#   dkms install limina-virtio-gpu/<v>
#   dracut -f                                    # ours into the initramfs (early KMS)
#   reboot
set -euo pipefail
cd "$(dirname "$0")/.."

KTAG="${1:-v6.19.10}"
# Version: kernel series it was vendored from + our patch level (bump on patch changes).
VERSION="${KTAG#v}.limina1"
NAME="limina-virtio-gpu"
OUT="target/virtio-gpu-dkms"
TREE="$OUT/$NAME-$VERSION"
BASE_URL="https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/plain/drivers/gpu/drm/virtio"

# The full in-tree source set (mirrors drivers/gpu/drm/virtio/Makefile for this series;
# keep in sync with guest/virtio-gpu-dkms/Kbuild).
FILES=(
    virtgpu_drv.c virtgpu_drv.h virtgpu_kms.c virtgpu_gem.c virtgpu_vram.c
    virtgpu_display.c virtgpu_vq.c virtgpu_fence.c virtgpu_object.c
    virtgpu_debugfs.c virtgpu_plane.c virtgpu_ioctl.c virtgpu_prime.c
    virtgpu_trace.h virtgpu_trace_points.c virtgpu_submit.c
)

rm -rf "$TREE"
mkdir -p "$TREE"

echo "==> vendoring virtio-gpu sources from stable $KTAG"
for f in "${FILES[@]}"; do
    curl -sfL "$BASE_URL/$f?h=$KTAG" -o "$TREE/$f" \
        || { echo "FAILED to fetch $f for $KTAG" >&2; exit 1; }
    # cgit serves an HTML error page for unknown refs; make sure we got C source.
    head -c 100 "$TREE/$f" | grep -q "<html" && { echo "bad fetch: $f ($KTAG)" >&2; exit 1; }
done

echo "==> applying the alignment patch"
# Patch paths are in-tree (a/drivers/gpu/drm/virtio/...); the sources sit at the tree
# root here, so strip 5 components.
patch -p5 -d "$TREE" --no-backup-if-mismatch \
    < guest/virtio-gpu-dkms/0001-align-host-visible-allocations-to-16-KiB.patch

echo "==> rewriting TRACE_INCLUDE_PATH for the out-of-tree layout"
# In-tree it's relative to include/trace; out of tree "." resolves via Kbuild's -I$(src).
sed -i '' 's|#define TRACE_INCLUDE_PATH \.\./\.\./drivers/gpu/drm/virtio|#define TRACE_INCLUDE_PATH .|' \
    "$TREE/virtgpu_trace.h"
grep -q "TRACE_INCLUDE_PATH \." "$TREE/virtgpu_trace.h" \
    || { echo "TRACE_INCLUDE_PATH rewrite failed (series layout changed?)" >&2; exit 1; }

echo "==> staging DKMS metadata"
cp guest/virtio-gpu-dkms/Kbuild guest/virtio-gpu-dkms/Makefile "$TREE/"
sed "s/@VERSION@/$VERSION/" guest/virtio-gpu-dkms/dkms.conf.in > "$TREE/dkms.conf"

tar -czf "$OUT/$NAME-$VERSION.tar.gz" -C "$OUT" "$NAME-$VERSION"
echo "==> DKMS tree ready: $TREE"
echo "    tarball:         $OUT/$NAME-$VERSION.tar.gz"
