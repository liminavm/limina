#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# virtiofs-16k-share spike: boot a scratch enhanced image (EFI + venus, same env as
# spikes/venus-draw-probe/boot-enhanced-efi-kk.sh) with one ro and one rw share, but with a
# spike-private worker log so a concurrently-running boot-enhanced-efi-kk.sh isn't clobbered.
#   LIMINA_DISK=host-sleep-eyeball.raw spikes/virtiofs-16k-share/boot-with-shares.sh
set -u
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
cd "$ROOT"
LOG="${LIMINA_LOG:-/tmp/virtiofs-16k-share-worker.log}"
WORK="${LIMINA_DISK:?set LIMINA_DISK to the scratch .raw}"
FW="${LIMINA_FIRMWARE:-target/krun-efi/KRUN_EFI.gop.fd}"
[ -f "$FW" ] || { echo "GOP firmware not found at $FW"; exit 1; }
ICD=$(ls /Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.*.json 2>/dev/null | head -1)
[ -n "$ICD" ] || { echo "no KosmicKrisp ICD under /Volumes/mesa-cs/build-kk"; exit 1; }
rm -f "$LOG"
MESA_PREFIX="${MESA_PREFIX:-/Volumes/mesa-cs/zink-kk-prefix}"
export VK_ICD_FILENAMES="$ICD"
export VK_DRIVER_FILES="$ICD"
export DYLD_FALLBACK_LIBRARY_PATH="$MESA_PREFIX/lib:$ROOT/third_party/epoxy-egl-prefix/lib:/opt/homebrew/lib${DYLD_FALLBACK_LIBRARY_PATH:+:$DYLD_FALLBACK_LIBRARY_PATH}"
export MESA_LOADER_DRIVER_OVERRIDE=zink
export GALLIUM_DRIVER=zink
export LIBGL_DRIVERS_PATH="$MESA_PREFIX/lib"
export EGL_PLATFORM=surfaceless
export VIRGL_LOG_LEVEL=info
export LIMINA_PRESENT_COPY=1
export RUST_LOG="${RUST_LOG:-debug}"
SPIKE=$ROOT/spikes/virtiofs-16k-share
target/debug/limina --vmm-bin "${LIMINA_VMM_BIN:-target/debug/limina-vmm}" \
  --firmware "$FW" \
  --disk "$WORK" --cpus 4 --ram-mib 6144 --net --window \
  --share tools="$SPIKE/share-ro":ro --share toolsrw="$SPIKE/share-rw" \
  >"$LOG" 2>&1 &
echo "limina pid=$! (worker log $LOG, disk $WORK)"
wait
