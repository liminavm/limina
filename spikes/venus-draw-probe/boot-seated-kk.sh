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
# Skip KK's per-draw GPU index-unroll for list topologies (round 17: GLES3/WebGL2 always-on
# primitive restart made EVERY indexed list draw pay ~10us of compute — 16fps -> 60fps on the
# aquarium). Policy default ON; LIMINA_KK_NOLISTRESTART=0 disables (the knob is presence-tested,
# so we only export it when enabled).
if [ "${LIMINA_KK_NOLISTRESTART-1}" = "0" ]; then
  unset LIMINA_KK_NOLISTRESTART
else
  export LIMINA_KK_NOLISTRESTART=1
fi
# Round-19 replay-thread relief (both default ON, =0 disables):
# - LIMINA_KK_BOCACHE: raise the cmd-pool free-BO cache cap 32 -> 512. A 10k-draw frame
#   burns ~350 128KiB upload BOs; the stock cap re-created ~300 Metal buffers per frame.
# - LIMINA_KK_SLIMPUSH: size push-descriptor uploads by the set layout instead of the
#   full 2 KiB array (zink pushes per draw). 420k -> ~550k draws/s on the aquarium.
if [ "${LIMINA_KK_BOCACHE-1}" = "0" ]; then unset LIMINA_KK_BOCACHE; else export LIMINA_KK_BOCACHE=1; fi
if [ "${LIMINA_KK_SLIMPUSH-1}" = "0" ]; then unset LIMINA_KK_SLIMPUSH; else export LIMINA_KK_SLIMPUSH=1; fi
# Drop KK's blanket injected FS depth-write + helper-quad sample-mask write, restoring
# early-Z/HSR (round 18: CTS A/B status-identical over the 10.9k early-Z-sensitive cases +
# human eyeball clean; kk-draw-bench fill 3x -> ~parity vs MVK). Policy default ON;
# LIMINA_KK_EARLYZ=0 disables (presence-tested knob, only exported when enabled).
if [ "${LIMINA_KK_EARLYZ-1}" = "0" ]; then
  unset LIMINA_KK_EARLYZ
else
  export LIMINA_KK_EARLYZ=1
fi
# KK debug levers (docs/drivers/kosmickrisp.rst): MESA_KK_DEBUG=msl logs generated MSL;
# MESA_KK_GPU_CAPTURE=1 arms Metal capture device-create..destroy.
[ -n "${MESA_KK_DEBUG:-}" ] && export MESA_KK_DEBUG
[ -n "${MESA_KK_GPU_CAPTURE:-}" ] && export MESA_KK_GPU_CAPTURE
# Round-21 flicker mitigation (default ON, =0 disables): present a private copy of the
# scanout to Core Animation instead of the live guest surface. The zero-copy present fires
# at flush (mutter's submit) time, before the GPU repaint executed; CA samples unsynced and
# can show the buffer's previous content (visible stale-frame flicker). IOSurfaceLock in
# the copy waits for pending GPU writes, so copied frames are always complete. Real fix =
# fence-accurate presents (roadmap #8). Toggle live via /tmp/limina-present-copy.
if [ "${LIMINA_PRESENT_COPY-1}" = "0" ]; then unset LIMINA_PRESENT_COPY; else export LIMINA_PRESENT_COPY=1; fi
# LIMINA_PRESENT_LOCK (lock-only, zero-copy): A/B FAILED 2026-06-11 — several anomalies within
# seconds (worse than untreated). The copy's immutable snapshot is the load-bearing property,
# not the GPU sync (repaint may be unsubmitted at present time; guest reuses the buffer while
# CA still samples). Kept only as the documented negative result — do not enable.
[ -n "${LIMINA_PRESENT_LOCK:-}" ] && export LIMINA_PRESENT_LOCK
# LIMINA_FENCE_PRESENT (#8 half 1, zero-copy, under A/B): the worker parks each scanout flush
# and presents only when a fence injected on the rendering context retires at TRUE GPU
# completion (vkr ring-decode barrier + per-queue zero-command submit; see fence-present
# design doc). Candidate replacement for the PRESENT_COPY stopgap on the display hop —
# A/B with LIMINA_PRESENT_COPY=0 LIMINA_FENCE_PRESENT=1. Live toggle: /tmp/limina-fence-present.
[ -n "${LIMINA_FENCE_PRESENT:-}" ] && export LIMINA_FENCE_PRESENT
NET_FLAG=--net
[ "${LIMINA_NET:-1}" = "0" ] && NET_FLAG=
target/debug/limina --vmm-bin target/debug/limina-vmm \
  --kernel target/test-guest/kernel/Image-16k \
  --cmdline "root=/dev/vda3 rootflags=subvol=root rootfstype=btrfs rw selinux=0 console=ttyAMA0" \
  --disk "$WORK" --cpus 4 --ram-mib 4096 $NET_FLAG --window \
  >"$LOG" 2>&1 &
echo "limina pid=$! (worker log $LOG, ICD $ICD)"
wait
