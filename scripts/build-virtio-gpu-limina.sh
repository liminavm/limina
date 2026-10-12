#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Assemble the limina-virtio-gpu-ext DKMS tree: the in-tree virtio-gpu DRM driver for a stable
# kernel series, plus limina's native-context extension (guest/virtio-gpu-limina, see the README
# there). Optionally compile it against a Fedora kernel in the limina-build container, which is
# how a guest gets the module without a toolchain of its own.
#
# Usage: scripts/build-virtio-gpu-limina.sh [KTAG] [KREL]
#   KTAG: stable kernel tag whose virtio-gpu sources to vendor (default v6.19.10, the F44 stock
#         kernel; see docs/images.md).
#   KREL: when given, also build the module against that Fedora kernel-devel release
#         (e.g. 6.19.10-300.fc44.aarch64) and leave it at target/virtio-gpu-limina/<KREL>/.
# Output: target/virtio-gpu-limina/limina-virtio-gpu-ext-<version>/ (the DKMS tree) and its
#         .tar.gz.
#
# Installing the prebuilt module in a guest:
#   install -D -m644 virtio-gpu.ko /lib/modules/$(uname -r)/extra/virtio-gpu.ko
#   depmod -a && dracut -f && reboot
set -euo pipefail
cd "$(dirname "$0")/.."

KTAG="${1:-v6.19.10}"
KREL="${2:-}"
VERSION="${KTAG#v}.limina1"
NAME="limina-virtio-gpu-ext"
SRC="guest/virtio-gpu-limina"
OUT="target/virtio-gpu-limina"
TREE="$OUT/$NAME-$VERSION"
BASE_URL="https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/plain/drivers/gpu/drm/virtio"

# The full in-tree source set; keep in sync with $SRC/Kbuild.
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

echo "==> hooking in the extension"
# Patch paths are in-tree (a/drivers/gpu/drm/virtio/...); strip to the tree root.
patch -p5 -d "$TREE" --no-backup-if-mismatch < "$SRC/0001-hook-the-limina-extension.patch"
cp "$SRC/virtgpu_limina.c" "$SRC/virtgpu_limina_drm.h" "$TREE/"

# In-tree TRACE_INCLUDE_PATH is relative to include/trace; out of tree "." resolves via -I$(src).
sed -i '' 's|#define TRACE_INCLUDE_PATH \.\./\.\./drivers/gpu/drm/virtio|#define TRACE_INCLUDE_PATH .|' \
    "$TREE/virtgpu_trace.h"
grep -q "TRACE_INCLUDE_PATH \." "$TREE/virtgpu_trace.h" \
    || { echo "TRACE_INCLUDE_PATH rewrite failed (series layout changed?)" >&2; exit 1; }

cp "$SRC/Kbuild" "$SRC/Makefile" "$TREE/"
sed "s/@VERSION@/$VERSION/" "$SRC/dkms.conf.in" > "$TREE/dkms.conf"
tar -czf "$OUT/$NAME-$VERSION.tar.gz" -C "$OUT" "$NAME-$VERSION"
echo "==> DKMS tree ready: $TREE"

[ -n "$KREL" ] || exit 0

# shellcheck source=scripts/build-image.sh
. scripts/build-image.sh
mkdir -p "$OUT/$KREL"
echo "==> building against kernel-devel-$KREL"
container run --rm --cpus 4 --memory 4g \
    -v "$(pwd)/$TREE:/src:ro" \
    -v "$(pwd)/$OUT/$KREL:/out" \
    "$LIMINA_BUILD_IMAGE" bash -euo pipefail -c "
        dnf -y -q install 'kernel-devel-$KREL' >/dev/null
        cp -r /src /tmp/build
        make -C '/usr/src/kernels/$KREL' M=/tmp/build W=1 modules
        cp /tmp/build/virtio-gpu.ko /out/
    "
echo "==> module: $OUT/$KREL/virtio-gpu.ko"
