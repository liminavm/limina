#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Run the 2026-09-16 virglrs legs in order, each through point.sh. The split is the virglrs-review
# session's: each leg is named by the virglrs tip it builds at, and the range is what it adds over
# the previous one. The libkrun rev is the one whose rutabaga follows that virglrs API:
#   b0  deb095b  debc082c            the pin, the baseline
#   l0  f0fdc66  debc082c + patch    resource-drop ownership, surface rename (already pushed)
#   l1  985d1da  bc045955            trust-boundary refusals + sampler dirt (9f7d930, da27493)
#   l2  bc4b864  3fa4cd9b            venus: census mutex, driver waits outside the batch, sliced waits
#   l3  1566e77  3fa4cd9b            submit-stats instruments + shader dirty on rasterizer bind
#   l4  ec5f1e9  3fa4cd9b            program reselect on dirt alone (BGRA targets)
#   l5  8d8a3d4  3fa4cd9b            scatter list walked once per transfer
#   l6  d833259  3fa4cd9b            transfers staged through one kept buffer
# then the baseline and the tip again (r0, r6), so drift over the night shows as a baseline that moved.
# An aborted point is logged and the run moves on; read the log for ABORTED, WEDGED and WATCHDOG.
# Usage: perf/virglrs-legs-2026-09-16/legs.sh [first-label]   (resume from a label)
set -uo pipefail
cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
D=perf/virglrs-legs-2026-09-16
PIN=debc082c27d0ac9cb0a1b0f3a90c9848c6ff0520
RENAME=bc045955
WAIT=3fa4cd9b
POINTS=(
  "b0-deb095b deb095b $PIN"
  "l0-f0fdc66 f0fdc66 $PIN $D/libkrun-debc082c-surface-rename.patch"
  "l1-985d1da 985d1da $RENAME"
  "l2-bc4b864 bc4b864 $WAIT"
  "l3-1566e77 1566e77 $WAIT"
  "l4-ec5f1e9 ec5f1e9 $WAIT"
  "l5-8d8a3d4 8d8a3d4 $WAIT"
  "l6-d833259 d833259 $WAIT"
  "r0-deb095b deb095b $PIN"
  "r6-d833259 d833259 $WAIT"
)
started=${1:+0}; started=${started:-1}
for p in "${POINTS[@]}"; do
  read -r label vrev lrev patch <<<"$p"
  [ "$started" = 1 ] || { [ "$label" = "$1" ] && started=1 || continue; }
  echo "######## $label $(date +%H:%M:%S)"
  LIBKRUN_PATCH="${patch:-}" $D/point.sh "$label" "$vrev" "$lrev" || echo "######## $label ABORTED rc=$?"
done
echo "######## legs done $(date +%H:%M:%S)"
