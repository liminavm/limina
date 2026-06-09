#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Boot the dev-enh guest to the SEATED gnome-shell desktop (graphical.target, autologin) with the
# INSTRUMENTED host MoltenVK loaded, to read the REAL indexed draws gnome-shell issues. This is the
# vehicle for the tier-2 "stretched-triangle / stale-icon" defect: a copy-based coherency probe
# (vkcoh-offset.c / vkcoh-churn.c) proved host-visible memory is fully coherent for transfer-queue
# buffer reads, so the defect is in the actual graphics indexed-draw path, not raw coherency.
#
# Set LIMINA_IDX_DUMP=1 (default here) → [LIMINA-IDX] per indexed draw: prim type, primitive-restart
# state, baseVertex, and an index-buffer scan (maxIdx, restart-sentinel count, first indices).
# Add LIMINA_VTX_DUMP=1 to also get [LIMINA-VTX] (vertex buffer len/stride → max valid index, to compare
# against [LIMINA-IDX] maxIdx for out-of-bounds fetch). Both are firehoses on a live desktop — capture
# a short window then analyse.
#
# NOTE: do NOT add MTL_SHADER_VALIDATION/MTL_DEBUG_LAYER (separate replaceRegion-stride worker abort).
set -u
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
cd "$ROOT"
LOG=/tmp/seated-mvkinst-worker.log
ICD=/tmp/mvk-instrumented/MoltenVK_icd.json
[ -f "$ICD" ] || { echo "no instrumented MoltenVK at $ICD — run spikes/venus-draw-probe/rebuild-mvk.sh first"; exit 1; }
# LIMINA_DISK=<path> reuses an already-prepared disk WITHOUT re-cloning — e.g. /tmp/seated-clean.raw with a
# disable-batching environment.d already injected, to instrument the WORKING (fixed) path. Default = fresh clone.
if [ -n "${LIMINA_DISK:-}" ]; then
  WORK="$LIMINA_DISK"; echo "reusing prepared disk $WORK (no clone)"
else
  WORK=/tmp/seated-mvkinst.raw
  rm -f "$WORK"; cp -c Fedora-Workstation-43.dev-enh.raw "$WORK" || { echo CLONE_FAIL; exit 1; }
fi
rm -f "$LOG"
# Export the instrument env cleanly (a bare ${VAR:+VAR=$VAR} prefix mis-parses as a command).
# LIMINA_IDX_DUMP defaults ON (the firehose); pass LIMINA_IDX_DUMP=0 to silence it for a DS-only differential.
export VK_ICD_FILENAMES="$ICD"
[ "${LIMINA_IDX_DUMP:-1}" != "0" ] && export LIMINA_IDX_DUMP=1
[ -n "${LIMINA_VTX_DUMP:-}" ] && export LIMINA_VTX_DUMP
[ -n "${LIMINA_DS_DUMP:-}" ] && export LIMINA_DS_DUMP   # [LIMINA-DS] depth/stencil differential (batched vs fan)
# --net is on by default (SSH into the guest); pass LIMINA_NET=0 to skip it — e.g. a worker-log-only
# differential (reading [LIMINA-DS]/[LIMINA-IDX] stderr) needs no guest network and avoids a stale-gvproxy
# port-2222 conflict.
NET_FLAG=--net
[ "${LIMINA_NET:-1}" = "0" ] && NET_FLAG=
target/debug/limina --vmm-bin target/debug/limina-vmm \
  --kernel target/test-guest/kernel/Image-16k \
  --cmdline "root=/dev/vda3 rootflags=subvol=root rootfstype=btrfs rw selinux=0 console=ttyAMA0" \
  --disk "$WORK" --cpus 4 --ram-mib 4096 $NET_FLAG --window \
  >"$LOG" 2>&1 &
echo "limina pid=$! (worker log $LOG)"
wait
