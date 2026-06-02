#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build a custom aarch64 Linux kernel Image for limina's L1 test guest, using Apple
# `container` as the (host-native) Linux build environment.
#
# Why a container: the host is macOS (case-insensitive FS — can't even check out the
# Linux tree) and has no kernel toolchain. We clone + build INSIDE an aarch64 Fedora
# container (case-sensitive ext4, native compile on Apple Silicon — no cross-compiler)
# and copy only the finished Image out to a bind-mounted host dir.
#
# This is the enhanced-tier kernel: our own config (virtio-fs root, vsock, real
# initramfs support, PL011 console). The libkrunfw kernel that build-test-guest.sh
# extracts remains the zero-dependency fallback.
#
# Usage: scripts/build-test-kernel.sh
#   KVER=v6.12  scripts/build-test-kernel.sh     # pin a different kernel tag
# Prereq: `container system start` (Apple container running). Network required.
set -euo pipefail
cd "$(dirname "$0")/.."

KVER="${KVER:-v6.12}"
JOBS="${JOBS:-8}"
MEM="${MEM:-8g}"
PAGESIZE="${PAGESIZE:-4k}"               # 4k (default) or 16k (matches the 16 KiB host)
OUT="target/test-guest/kernel"          # gitignored (under target/)
mkdir -p "$OUT"

# Default 4k kernel is the L1 default (-> Image, picked up by build-test-guest.sh).
# A 16k kernel goes to a distinct name so it doesn't clobber the default.
case "$PAGESIZE" in
    4k)  OUT_NAME="Image"; PAGE_CONFIG="CONFIG_ARM64_4K_PAGES=y" ;;
    16k) OUT_NAME="Image-16k"; PAGE_CONFIG="CONFIG_ARM64_16K_PAGES=y" ;;
    *)   echo "PAGESIZE must be 4k or 16k (got '$PAGESIZE')" >&2; exit 1 ;;
esac

command -v container >/dev/null || { echo "Apple 'container' not installed (brew install container)" >&2; exit 1; }

# limina kernel config fragment (merged onto arm64 defconfig). Everything libkrun's guest
# needs: virtio-mmio transport, virtio-fs root (FUSE), vsock for the future agent, real
# initramfs support, PL011 serial console, devtmpfs auto-mount. BTF off (no pahole dep).
cat > "$OUT/limina.fragment" <<'FRAG'
CONFIG_VIRTIO=y
CONFIG_VIRTIO_MMIO=y
CONFIG_VIRTIO_BLK=y
CONFIG_VIRTIO_CONSOLE=y
CONFIG_FUSE_FS=y
CONFIG_VIRTIO_FS=y
CONFIG_VSOCKETS=y
CONFIG_VIRTIO_VSOCKETS=y
CONFIG_BLK_DEV_INITRD=y
CONFIG_DEVTMPFS=y
CONFIG_DEVTMPFS_MOUNT=y
CONFIG_SERIAL_AMBA_PL011=y
CONFIG_SERIAL_AMBA_PL011_CONSOLE=y
CONFIG_DEBUG_INFO_BTF=n
FRAG
echo "$PAGE_CONFIG" >> "$OUT/limina.fragment"   # page-size choice (4k default / 16k)

DEPS="gcc make flex bison bc elfutils-libelf-devel openssl-devel perl findutils diffutils gzip xz cpio git rsync kmod python3"

echo "==> building Linux $KVER (arm64, $PAGESIZE pages, -j$JOBS) in an Apple container"
container run --rm --cpus "$JOBS" --memory "$MEM" \
    -v "$(pwd)/$OUT:/out" \
    docker.io/library/fedora:43 bash -euo pipefail -c "
        OUT_NAME='$OUT_NAME'
        echo '--- installing build deps'
        dnf -y install $DEPS >/dev/null
        # Cache the source as a BARE repo on the bind-mounted host dir (bare = no
        # worktree → no case-collisions on the case-insensitive host FS). The worktree
        # is a fast local clone onto the container's ext4. Re-runs skip the download.
        CACHE=/out/cache/linux-$KVER.git
        if [ ! -d \"\$CACHE\" ]; then
            echo '--- shallow-cloning $KVER into cache (first run)'
            mkdir -p /out/cache
            git clone --bare --depth 1 --branch '$KVER' https://github.com/torvalds/linux \"\$CACHE\"
        else
            echo '--- reusing cached $KVER source'
        fi
        git clone \"\$CACHE\" /tmp/linux
        cd /tmp/linux
        make ARCH=arm64 defconfig
        ./scripts/kconfig/merge_config.sh -m .config /out/limina.fragment
        make ARCH=arm64 olddefconfig
        echo '--- verifying key options survived'
        for opt in CONFIG_VIRTIO_FS CONFIG_BLK_DEV_INITRD CONFIG_SERIAL_AMBA_PL011_CONSOLE CONFIG_VIRTIO_VSOCKETS; do
            grep -q \"^\$opt=y\" .config || { echo \"MISSING \$opt\" >&2; exit 1; }
        done
        grep -q '^$PAGE_CONFIG' .config || { echo 'MISSING $PAGE_CONFIG' >&2; exit 1; }
        echo '--- compiling Image'
        make ARCH=arm64 -j\"\$(nproc)\" Image
        cp -f arch/arm64/boot/Image \"/out/\$OUT_NAME\"
        echo \"--- done: \$(wc -c < /out/\$OUT_NAME) bytes\"
    "

echo "==> custom kernel ready: $OUT/$OUT_NAME"
echo "    boot it via: LIMINA_TEST_KERNEL=$OUT/$OUT_NAME  (L1 test)"
