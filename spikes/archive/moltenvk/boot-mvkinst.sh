#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Boot the dev-enh guest to multi-user.target (no GNOME → card0 DRM master free, no GL noise) with the
# INSTRUMENTED host MoltenVK loaded (built by rebuild-mvk.sh). The worker log then shows [LIMINA-DRAW] /
# [LIMINA-VTX] for whatever GL app you run in the guest (e.g. tri.c). Multi-user keeps the only GL
# activity = your probe, so the MoltenVK log is clean.
#
# Usage:  spikes/venus-draw-probe/boot-mvkinst.sh        # boots in foreground; drive via ssh -p 2222
# Then in the guest (patched zink env, see limina-fedora-access memo): build+run tri/occ, and on the HOST
# grep /tmp/mvkinst-worker.log for LIMINA-DRAW/LIMINA-VTX.
#
# NOTE: do NOT add MTL_SHADER_VALIDATION/MTL_DEBUG_LAYER — they abort the worker on a separate
# (non-bug-A) replaceRegion stride defect in MoltenVK's linear-image host-sync (#28-adjacent).
set -u
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
cd "$ROOT"
WORK=/tmp/kmscube-isolate.raw
LOG=/tmp/mvkinst-worker.log
ICD=/tmp/mvk-instrumented/MoltenVK_icd.json
[ -f "$ICD" ] || { echo "no instrumented MoltenVK at $ICD — run spikes/venus-draw-probe/rebuild-mvk.sh first"; exit 1; }
rm -f "$WORK"; cp -c Fedora-Workstation-43.dev-enh.raw "$WORK" || { echo CLONE_FAIL; exit 1; }
rm -f "$LOG"
VK_ICD_FILENAMES="$ICD" \
target/debug/limina --vmm-bin target/debug/limina-vmm \
  --kernel target/test-guest/kernel/Image-16k \
  --cmdline "root=/dev/vda3 rootflags=subvol=root rootfstype=btrfs rw selinux=0 console=ttyAMA0 systemd.unit=multi-user.target" \
  --disk "$WORK" --cpus 4 --ram-mib 4096 --net --window \
  >"$LOG" 2>&1 &
echo "limina pid=$! (worker log $LOG)"
wait
