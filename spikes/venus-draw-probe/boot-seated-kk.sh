#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Boot the dev-enh guest to the SEATED desktop with KOSMICKRISP (mesa Vulkan-on-Metal,
# /Volumes/mesa-cs/build-kk) as the host Vulkan driver instead of MoltenVK — the A/B vehicle
# for evaluating KK as a MoltenVK alternative under virglrenderer/venus.
#
# Same plumbing as boot-seated-mvkinst.sh: the render server picks its Vulkan driver via
# VK_ICD_FILENAMES. Build KK first (see docs/drivers/kosmickrisp.rst; deps via brew, mesa
# checkout must live on the case-sensitive volume third_party/mesa-cs.sparseimage).
#
# LIMINA_DISK=<path> reuses a prepared disk without re-cloning (e.g. the golden dev-enh.raw).
set -u
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
cd "$ROOT"
LOG=/tmp/seated-kk-worker.log
ICD=$(ls /Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.*.json 2>/dev/null | head -1)
[ -n "$ICD" ] || { echo "no kosmickrisp ICD under /Volumes/mesa-cs/build-kk — mount third_party/mesa-cs.sparseimage and ninja first"; exit 1; }
if [ -n "${LIMINA_DISK:-}" ]; then
  WORK="$LIMINA_DISK"; echo "reusing prepared disk $WORK (no clone)"
else
  WORK=/tmp/seated-kk.raw
  rm -f "$WORK"; cp -c Fedora-Workstation-43.dev-enh.raw "$WORK" || { echo CLONE_FAIL; exit 1; }
fi
rm -f "$LOG"
export VK_ICD_FILENAMES="$ICD"
# vkr_log emits at virgl INFO; the default logger level is WARNING, which silently
# swallows every limina: line in vkr_*.c — keep INFO on for this A/B vehicle.
export VIRGL_LOG_LEVEL=info
# KK debug levers (docs/drivers/kosmickrisp.rst): MESA_KK_DEBUG=msl logs generated MSL;
# MESA_KK_GPU_CAPTURE=1 arms Metal capture device-create..destroy.
[ -n "${MESA_KK_DEBUG:-}" ] && export MESA_KK_DEBUG
[ -n "${MESA_KK_GPU_CAPTURE:-}" ] && export MESA_KK_GPU_CAPTURE
NET_FLAG=--net
[ "${LIMINA_NET:-1}" = "0" ] && NET_FLAG=
target/debug/limina --vmm-bin target/debug/limina-vmm \
  --kernel target/test-guest/kernel/Image-16k \
  --cmdline "root=/dev/vda3 rootflags=subvol=root rootfstype=btrfs rw selinux=0 console=ttyAMA0" \
  --disk "$WORK" --cpus 4 --ram-mib 4096 $NET_FLAG --window \
  >"$LOG" 2>&1 &
echo "limina pid=$! (worker log $LOG, ICD $ICD)"
wait
