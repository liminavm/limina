#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Boot the ENHANCED tier (custom 16 KiB-page kernel direct-booting Fedora's btrfs root,
# coexist Venus GPU + user-mode NAT) in a native limina window, so you can VISUALLY verify
# 3D acceleration. Pair with scripts/venus-gl-test.sh (run it in another terminal once the
# desktop is up) to force a GL app onto zink→Venus→MoltenVK→Apple GPU and read its renderer.
#
# Why the 16 KiB kernel: a 4 KiB-page guest places Venus host-visible blobs at 4 KiB
# granularity, which a 16 KiB host can't hv_vm_map → stock Fedora falls back to llvmpipe. A
# 16 KiB guest places them on 16 KiB boundaries and Venus enumerates the real GPU. See
# docs/roadmap.md M4 / memory limina-tier2-venus.
#
# NOTE (2026-06-07): GL rendering on Venus WORKS — glmark2-wayland forced onto zink→Venus scores
# ~445 on the Apple M1 Max GPU (task #27 fence retirement fixed via the macOS eventfd shim, with
# Venus host-visible *feedback* disabled — see scripts/venus-gl-test.sh / VN_PERF). The normal
# GNOME desktop (gnome-shell) still runs on llvmpipe by default; use venus-gl-test.sh in a second
# terminal to launch an app onto Venus and watch it render in this window.
#
# Prereqs:
#   - build the kernel:  scripts/build-test-kernel.sh PAGESIZE=16k   (-> Image-16k)
#   - build the 1.3.0 Venus virglrenderer:  scripts/build-virglrenderer.sh
#   - build + sign limina against it (PKG_CONFIG_PATH=third_party/virgl-prefix/lib/pkgconfig:...)
#
# Usage: scripts/run-venus-window.sh [debug|release]
#   Env: LIMINA_TEST_KERNEL_16K (default target/test-guest/kernel/Image-16k),
#        LIMINA_DISK (default Fedora-Workstation-43.raw), LIMINA_FEDORA_RAM_MIB (default 4096),
#        LIMINA_FEDORA_CPUS (default 4), LIMINA_DISPLAY_SIZE (default 1280x800).
set -euo pipefail
cd "$(dirname "$0")/.."

PROFILE="${1:-debug}"
KERNEL="${LIMINA_TEST_KERNEL_16K:-target/test-guest/kernel/Image-16k}"
DISK_SRC="${LIMINA_DISK:-Fedora-Workstation-43.raw}"
RAM="${LIMINA_FEDORA_RAM_MIB:-4096}"
CPUS="${LIMINA_FEDORA_CPUS:-4}"
SIZE="${LIMINA_DISPLAY_SIZE:-1280x800}"

[ -f "$KERNEL" ]   || { echo "16 KiB kernel missing: $KERNEL (scripts/build-test-kernel.sh PAGESIZE=16k)" >&2; exit 1; }
[ -f "$DISK_SRC" ] || { echo "disk missing: $DISK_SRC (set LIMINA_DISK)" >&2; exit 1; }

# Never mutate the pristine image: boot a writable APFS COW clone (cp -c: instant). Kept for
# the whole run so in-guest state (e.g. /opt/mesa-zink) is reachable over SSH, discarded on exit.
DISK="$(mktemp -d)/fedora-venus.raw"
cp -c "$DISK_SRC" "$DISK"
trap 'rm -rf "$(dirname "$DISK")"' EXIT

# Guard: a worker linked to Homebrew virglrenderer silently degrades to software-2D (no venus).
"$(dirname "$0")/check-virgl-link.sh" "target/$PROFILE/limina-vmm"

echo "==> enhanced/Venus tier: $KERNEL + $DISK_SRC (cow clone), coexist Venus + NAT, ${CPUS}vcpu/${RAM}MiB/$SIZE"
echo "    Once the GNOME desktop is up, in ANOTHER terminal run:"
echo "        scripts/venus-gl-test.sh            # glmark2 on zink→Venus, prints GL_RENDERER"
echo "        scripts/venus-gl-test.sh glxgears   # or glxgears / vkcube / glinfo"
echo "    SSH:  ssh -p 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null claude@127.0.0.1"
echo "    (close the window or Ctrl-C to quit)"
exec "target/$PROFILE/limina" --vmm-bin "target/$PROFILE/limina-vmm" \
    --kernel "$KERNEL" \
    --cmdline "root=/dev/vda3 rootflags=subvol=root rootfstype=btrfs rw selinux=0 console=ttyAMA0" \
    --disk "$DISK" --cpus "$CPUS" --ram-mib "$RAM" --net \
    --window --display-size "$SIZE"
