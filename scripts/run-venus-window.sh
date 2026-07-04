#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# ┌─ FRINGE BOOT MODE — NOT the default. ──────────────────────────────────────────────────────┐
# │ This --kernel-direct-boots an external Image-16k with selinux=0, bypassing the guest's GRUB. │
# │ For normal image boot/validation use EFI+venus (boots the guest's OWN installed kernel):     │
# │   LIMINA_DISK=<enhanced.raw> spikes/venus-draw-probe/boot-enhanced-efi-kk.sh                 │
# │ Reach for this direct-boot script only when you specifically need the injected test kernel.  │
# └─────────────────────────────────────────────────────────────────────────────────────────────┘
# Boot the ENHANCED tier (custom 16 KiB-page kernel direct-booting Fedora's btrfs root,
# coexist Venus GPU + user-mode NAT) in a native limina window, so you can VISUALLY verify
# 3D acceleration. Pair with scripts/venus-gl-test.sh (run it in another terminal once the
# desktop is up) to force a GL app onto zink→Venus→KosmicKrisp→Apple GPU and read its renderer.
#
# HOST VULKAN BACKEND: KosmicKrisp (KK) ONLY. venus is driven on the host by a Vulkan driver;
# we support exactly one — KK (mesa Vulkan-on-Metal). MoltenVK is NOT supported: its venus path
# corrupts the guest compositor (the #28 coherency / #32 stencil class of bugs KK fixed), so
# gnome-shell SIGSEGV-loops on it. This script forces VK_ICD_FILENAMES to the KK ICD and refuses
# to boot venus without it (rather than silently falling through to the loader's MoltenVK
# default). Build KK first (docs/drivers/kosmickrisp.rst; mount third_party/mesa-cs.sparseimage).
#
# Why the 16 KiB kernel: a 4 KiB-page guest places Venus host-visible blobs at 4 KiB
# granularity, which a 16 KiB host can't hv_vm_map → stock Fedora falls back to llvmpipe. A
# 16 KiB guest places them on 16 KiB boundaries and Venus enumerates the real GPU. See
# docs/roadmap.md M4 / memory limina-tier2-venus.
#
# NOTE (2026-06-07): GL rendering on Venus WORKS — glmark2-wayland forced onto zink→Venus scores
# ~445 on the Apple M1 Max GPU (task #27 fence retirement fixed via the macOS eventfd shim, with
# Venus host-visible *feedback* disabled — see scripts/venus-gl-test.sh / VN_PERF). With KK the
# full gnome-shell desktop runs on Venus too; use venus-gl-test.sh in a second terminal to force a
# specific app onto Venus and watch it render in this window.
#
# Prereqs:
#   - build the kernel:  scripts/build-test-kernel.sh PAGESIZE=16k   (-> Image-16k)
#   - build the 1.3.0 Venus virglrenderer:  scripts/build-virglrenderer.sh
#   - build + sign limina against it (PKG_CONFIG_PATH=third_party/virgl-prefix/lib/pkgconfig:...)
#   - build KosmicKrisp (the host Vulkan backend) and mount third_party/mesa-cs.sparseimage
#     (docs/drivers/kosmickrisp.rst) — required; MoltenVK is not supported.
#
# Usage: scripts/run-venus-window.sh [debug|release]
#   Env: LIMINA_TEST_KERNEL_16K (default target/test-guest/kernel/Image-16k),
#        LIMINA_DISK (default Fedora-Workstation-43.raw), LIMINA_FEDORA_RAM_MIB (default 4096),
#        LIMINA_FEDORA_CPUS (default 4), LIMINA_DISPLAY_SIZE (default 1280x800),
#        VK_ICD_FILENAMES (override the auto-resolved KosmicKrisp ICD).
set -euo pipefail
cd "$(dirname "$0")/.."

PROFILE="${1:-debug}"
KERNEL="${LIMINA_TEST_KERNEL_16K:-target/test-guest/kernel/Image-16k}"
DISK_SRC="${LIMINA_DISK:-Fedora-Workstation-${LIMINA_FEDORA_REL:-43}.enhanced.raw}"
RAM="${LIMINA_FEDORA_RAM_MIB:-4096}"
CPUS="${LIMINA_FEDORA_CPUS:-4}"
SIZE="${LIMINA_DISPLAY_SIZE:-1280x800}"

[ -f "$KERNEL" ]   || { echo "16 KiB kernel missing: $KERNEL (scripts/build-test-kernel.sh PAGESIZE=16k)" >&2; exit 1; }
[ -f "$DISK_SRC" ] || { echo "disk missing: $DISK_SRC (set LIMINA_DISK)" >&2; exit 1; }

# Host Vulkan backend: KosmicKrisp ONLY. Resolve the KK ICD (or honor an explicit override) and
# force it on the worker. Refuse to boot venus on MoltenVK — its venus path crashes the guest
# compositor (gnome-shell SIGSEGV-loop), so a missing KK is a hard error, not a silent fallback.
if [ -z "${VK_ICD_FILENAMES:-}" ]; then
    VK_ICD_FILENAMES="$(ls /Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.*.json 2>/dev/null | head -1)"
fi
[ -n "${VK_ICD_FILENAMES:-}" ] && [ -f "$VK_ICD_FILENAMES" ] || {
    echo "KosmicKrisp ICD not found under /Volumes/mesa-cs/build-kk — venus needs KK (MoltenVK is" >&2
    echo "not supported). Build KK and mount third_party/mesa-cs.sparseimage (docs/drivers/kosmickrisp.rst)," >&2
    echo "or set VK_ICD_FILENAMES to a KK ICD. For a no-3D desktop use scripts/run-fedora-window.sh." >&2
    exit 1
}
export VK_ICD_FILENAMES

# Never mutate the pristine image: boot a writable APFS COW clone (cp -c: instant). Kept for
# the whole run so in-guest state (e.g. /opt/mesa-zink) is reachable over SSH, discarded on exit.
DISK="$(mktemp -d)/fedora-venus.raw"
cp -c "$DISK_SRC" "$DISK"
trap 'rm -rf "$(dirname "$DISK")"' EXIT

# Guard: a worker linked to Homebrew virglrenderer silently degrades to software-2D (no venus).
"$(dirname "$0")/check-virgl-link.sh" "target/$PROFILE/limina-vmm"

echo "==> enhanced/Venus tier: $KERNEL + $DISK_SRC (cow clone), coexist Venus + NAT, ${CPUS}vcpu/${RAM}MiB/$SIZE"
echo "    host Vulkan backend (KosmicKrisp): $VK_ICD_FILENAMES"
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
