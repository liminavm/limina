#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# The guest half of the 2026-10-02 rebase as one A/B on the rebased host (point.sh says how the
# host half is read):
#   o  old  the enhanced image before payload r28 (mesa 26.1.8, kernel 7.1.8)
#   n  new  the enhanced image on payload r30 (mesa 26.2.3-2, kernel 7.1.13)
#
# Follow-up points for the aquarium 25k readings of 3 and 16 fps in n1-new: two more new
# points with an old one between them, so a repeat on the new guest is told apart from host drift.

# An aborted point is logged and the run moves on; read the log for ABORTED, WEDGED and WATCHDOG.
# Usage: perf/rebase-2026-10-02/legs-followup.sh [first-label]   (resume from a label)
set -uo pipefail
cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
D=perf/rebase-2026-10-02
POINTS=(
  "n2-new new"
  "o3-old old"
  "n3-new new"
)
started=${1:+0}; started=${started:-1}
for p in "${POINTS[@]}"; do
  read -r label arm <<<"$p"
  [ "$started" = 1 ] || { [ "$label" = "$1" ] && started=1 || continue; }
  echo "######## $label $(date +%H:%M:%S)"
  $D/point.sh "$label" "$arm" || echo "######## $label ABORTED rc=$?"
done
echo "######## legs done $(date +%H:%M:%S)"
