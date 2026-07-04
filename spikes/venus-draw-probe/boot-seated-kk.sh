#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# ┌─ FRINGE BOOT MODE — NOT the default. ──────────────────────────────────────────────────────┐
# │ This --kernel-INJECTS an external Image-16k with selinux=0, bypassing the guest's GRUB and   │
# │ SELinux. It is the deterministic test-kernel vehicle (the L2 venus tests wire it in) and a   │
# │ low-level kernel/early-boot debug tool — NOT how an image really runs.                       │
# │ To boot/validate an image normally, use EFI+venus instead:                                   │
# │   LIMINA_DISK=<enhanced.raw> spikes/venus-draw-probe/boot-enhanced-efi-kk.sh                 │
# │ (boots the guest's OWN installed kernel via GOP firmware→GRUB, enforcing, KK venus). Only    │
# │ reach for this script when you specifically need the injected test kernel. See CLAUDE.md.    │
# └─────────────────────────────────────────────────────────────────────────────────────────────┘
# Boot the dev-enh guest to the SEATED desktop with KOSMICKRISP (mesa Vulkan-on-Metal,
# /Volumes/mesa-cs/build-kk) as the host Vulkan driver instead of MoltenVK — the A/B vehicle
# for evaluating KK as a MoltenVK alternative under virglrenderer/venus.
#
# The render server picks its Vulkan driver via VK_ICD_FILENAMES; this points it at KK, limina's
# one supported venus backend (MoltenVK was retired 2026-06-13 — archived under
# spikes/archive/moltenvk/). Build KK first (see docs/drivers/kosmickrisp.rst; deps via brew, mesa
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
# The coexist device's vrend half provides host GL via zink-on-KK; it needs the zink-on-KK Mesa's
# libEGL (epoxy dlopens it by BARE name → DYLD path) AND the Mesa loader/gallium env that selects
# zink + surfaceless EGL. Without the latter the EGL DRI2 screen fails to create and
# virgl_renderer_init degrades the whole coexist device to software-2D (so venus never enumerates).
# /bin/bash is SIP-restricted and strips DYLD_* at startup, so a DYLD export from the calling shell
# never survives to here — set it now; the limina worker is codesigned with
# com.apple.security.cs.allow-dyld-environment-variables (non-restricted supervisor in between) so
# it honors it. Matches spikes/virgl-zink-kk/boot-virgl-windowed.sh + scripts/build-virglrenderer.sh.
MESA_PREFIX="${MESA_PREFIX:-/Volumes/mesa-cs/zink-kk-prefix}"
export DYLD_FALLBACK_LIBRARY_PATH="$MESA_PREFIX/lib:$ROOT/third_party/epoxy-egl-prefix/lib:/opt/homebrew/lib${DYLD_FALLBACK_LIBRARY_PATH:+:$DYLD_FALLBACK_LIBRARY_PATH}"
export VK_DRIVER_FILES="$ICD"
export MESA_LOADER_DRIVER_OVERRIDE=zink
export GALLIUM_DRIVER=zink
export LIBGL_DRIVERS_PATH="$MESA_PREFIX/lib"
export EGL_PLATFORM=surfaceless
# vkr_log emits at virgl INFO; the default logger level is WARNING, which silently
# swallows every limina: line in vkr_*.c — keep INFO on for this A/B vehicle.
export VIRGL_LOG_LEVEL=info
# KK perf knobs are DEFAULT-ON IN KK ITSELF since 2026-06-11 (kk-perf/kk-xfb patches);
# set LIMINA_KK_<X>=0 in the environment to disable one — the script just passes them through.
# - LIMINA_KK_NOLISTRESTART: skip the per-draw GPU index-unroll for list topologies
#   (round 17: zink's GLES3 always-on restart made every indexed list draw pay ~10us
#   of compute; 16fps -> 60fps on the aquarium).
# - LIMINA_KK_BOCACHE: cmd-pool free-BO cache cap 32 -> 512 (round 19: ~300 Metal buffer
#   create/destroys per 10k-draw frame). Numeric value >= 64 sets a custom cap.
# - LIMINA_KK_SLIMPUSH: size push-descriptor uploads by the per-set high-water mark
#   instead of the full 2 KiB array (round 19; 420k -> ~550k draws/s).
# - LIMINA_KK_EARLYZ: drop the blanket injected FS depth-write + helper-quad sample-mask
#   write, restoring early-Z/HSR (round 18: 10.9k-case CTS A/B status-identical).
# - LIMINA_KK_SLIMROOT: upload only the root-table prefix the bound shaders can address
#   (~900B vs ~2.2KiB per draw on zink-shaped pipelines; round 28: bench rebind leg
#   +43%, 14.7k-case CTS A/B status-identical).
# - LIMINA_KK_FASTBIND: per-encoder bind cache for the hot per-draw buffer binds (root
#   idx0, per-draw data idx2) — setBufferOffset / skip instead of full setBuffer, and
#   per-draw-data upload dedup (round 28b).
for knob in LIMINA_KK_NOLISTRESTART LIMINA_KK_BOCACHE LIMINA_KK_SLIMPUSH LIMINA_KK_EARLYZ LIMINA_KK_SLIMROOT LIMINA_KK_FASTBIND; do
  [ -n "$(eval echo "\${$knob:-}")" ] && export "$knob"
done
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
# LIMINA_FENCE_PRESENT (#8 half 1): present only at TRUE GPU completion (vkr ring-decode
# barrier + per-queue zero-command submit). A/B 2026-06-11: NOT sufficient alone — 4
# anomalies/58s. Presents are provably complete, so the survivor is the REUSE race (guest
# flip events are fake; mutter repaints ~33ms after flush while CA still samples; deferral
# widens the window). Needed as the foundation for half 2 (guest kernel fences blob
# flushes; host signals at present+latch). Until then: keep PRESENT_COPY on instead.
[ -n "${LIMINA_FENCE_PRESENT:-}" ] && export LIMINA_FENCE_PRESENT
NET_FLAG=--net
[ "${LIMINA_NET:-1}" = "0" ] && NET_FLAG=
# LIMINA_SHARE=<[NAME=]PATH[:ro]> attaches a host-directory share (M5 virtiofs;
# the baked limina-agent auto-mounts it at /media/<NAME> in the guest).
SHARE_FLAGS=()
[ -n "${LIMINA_SHARE:-}" ] && SHARE_FLAGS=(--share "$LIMINA_SHARE")
# LIMINA_EXTRA_ARGS=<flags> passes arbitrary extra limina flags through (e.g.
# --swap-cmd-opt for the M8 keymap test). Word-split intentionally.
EXTRA_ARGS=()
[ -n "${LIMINA_EXTRA_ARGS:-}" ] && read -ra EXTRA_ARGS <<<"$LIMINA_EXTRA_ARGS"
target/debug/limina --vmm-bin target/debug/limina-vmm \
  --kernel target/test-guest/kernel/Image-16k \
  --cmdline "root=/dev/vda3 rootflags=subvol=root rootfstype=btrfs rw selinux=0 console=ttyAMA0" \
  --disk "$WORK" --cpus 4 --ram-mib 4096 $NET_FLAG --window \
  ${SHARE_FLAGS[@]+"${SHARE_FLAGS[@]}"} \
  ${EXTRA_ARGS[@]+"${EXTRA_ARGS[@]}"} \
  >"$LOG" 2>&1 &
echo "limina pid=$! (worker log $LOG, ICD $ICD)"
wait
