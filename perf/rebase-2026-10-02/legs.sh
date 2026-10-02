#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# The guest half of the 2026-10-02 rebase as one A/B on the rebased host (point.sh says how the
# host half is read):
#   o  old  the enhanced image before payload r28 (mesa 26.1.8, kernel 7.1.8)
#   n  new  the enhanced image on payload r29 (mesa 26.2.3, kernel 7.1.13)
#
# Order is o0 n0 o1 n1 o2 -- alternating, with the baseline measured at BOTH ENDS, because the
# host's renderer speed drifts for tens of minutes and a one-ended sweep books that drift as the
# treatment's effect (perf/virglrs-vrend-2026-09-17/legs.sh).
#
# An aborted point is logged and the run moves on; read the log for ABORTED, WEDGED and WATCHDOG.
# Usage: perf/rebase-2026-10-02/legs.sh [first-label]   (resume from a label)
set -uo pipefail
cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
D=perf/rebase-2026-10-02
POINTS=(
  "o0-old old"
  "n0-new new"
  "o1-old old"
  "n1-new new"
  "o2-old old"
)
started=${1:+0}; started=${started:-1}
for p in "${POINTS[@]}"; do
  read -r label arm <<<"$p"
  [ "$started" = 1 ] || { [ "$label" = "$1" ] && started=1 || continue; }
  echo "######## $label $(date +%H:%M:%S)"
  $D/point.sh "$label" "$arm" || echo "######## $label ABORTED rc=$?"
done
echo "######## legs done $(date +%H:%M:%S)"
