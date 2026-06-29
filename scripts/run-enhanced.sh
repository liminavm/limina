#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Boot the in-repo Fedora image on limina's ENHANCED TIER: our custom 16 KiB-page kernel
# direct-booting Fedora's btrfs root, with the coexist (venus) GPU + user-mode NAT.
#
# Why the 16 KiB kernel: a 4 KiB-page guest places host-visible virtio-gpu (venus) blobs at
# 4 KiB granularity, which a 16 KiB host cannot map with hv_vm_map — so stock Fedora falls
# back to llvmpipe. A 16 KiB guest places them on 16 KiB boundaries and venus enumerates the
# real GPU. See docs/roadmap.md M4 / memory limina-tier2-venus.
#
# Prereqs:
#   - build the kernel:  scripts/build-test-kernel.sh PAGESIZE=16k   (-> Image-16k)
#   - build + sign limina: cargo build -p limina -p limina-vmm && crates/limina-vmm/sign.sh debug
#
# Usage: scripts/run-enhanced.sh [--window | --capture <png>]
#   default sink is --window (a native window); --capture writes frames to a PNG headlessly.
# Then: ssh -p 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null claude@127.0.0.1
set -euo pipefail
cd "$(dirname "$0")/.."

PROFILE="${PROFILE:-debug}"
KERNEL="${LIMINA_TEST_KERNEL_16K:-target/test-guest/kernel/Image-16k}"
DISK_SRC="${LIMINA_DISK:-Fedora-Workstation-${LIMINA_FEDORA_REL:-43}.enhanced.raw}"
SINK=(--window)
case "${1:-}" in
    --capture) SINK=(--display-capture "${2:?--capture needs a PNG path}" --display-size 1280x800) ;;
    --window|"") SINK=(--window) ;;
    *) echo "usage: $0 [--window | --capture <png>]" >&2; exit 1 ;;
esac

[ -f "$KERNEL" ] || { echo "16 KiB kernel missing: $KERNEL (run scripts/build-test-kernel.sh PAGESIZE=16k)" >&2; exit 1; }
[ -f "$DISK_SRC" ] || { echo "disk missing: $DISK_SRC" >&2; exit 1; }

# Never mutate the pristine image: boot a writable APFS COW clone (cp -c: instant).
DISK="$(mktemp -d)/fedora-enhanced.raw"
cp -c "$DISK_SRC" "$DISK"
trap 'rm -rf "$(dirname "$DISK")"' EXIT

# Guard: a worker linked to Homebrew virglrenderer silently degrades to software-2D (no venus).
"$(dirname "$0")/check-virgl-link.sh" "target/$PROFILE/limina-vmm"

echo "==> enhanced tier: $KERNEL + $DISK_SRC (cow clone), coexist venus + NAT"
exec "target/$PROFILE/limina" --vmm-bin "target/$PROFILE/limina-vmm" \
    --kernel "$KERNEL" \
    --cmdline "root=/dev/vda3 rootflags=subvol=root rootfstype=btrfs rw selinux=0 console=ttyAMA0" \
    --disk "$DISK" --cpus 4 --ram-mib 4096 --net \
    "${SINK[@]}"
