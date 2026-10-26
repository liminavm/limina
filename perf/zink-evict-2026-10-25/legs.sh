#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# The zink buffer-view eviction pass (point.sh says what the arms and tiers are). Per tier the
# order is old new old -- the baseline at BOTH ENDS, because the host's renderer speed drifts for
# tens of minutes and a one-ended sweep books that drift as the treatment's effect.
#
# An aborted point is logged and the run moves on; read the log for ABORTED, WEDGED and WATCHDOG.
# Usage: perf/zink-evict-2026-10-25/legs.sh [first-label]   (resume from a label)
set -uo pipefail
cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
D=perf/zink-evict-2026-10-25
POINTS=(
  "mb0-old old metal"
  "ma0-new new metal"
  "mb1-old old metal"
  "vb0-old old vrend"
  "va0-new new vrend"
  "vb1-old old vrend"
)
started=${1:+0}; started=${started:-1}
for p in "${POINTS[@]}"; do
  read -r label arm tier <<<"$p"
  [ "$started" = 1 ] || { [ "$label" = "$1" ] && started=1 || continue; }
  echo "######## $label $(date +%H:%M:%S)"
  $D/point.sh "$label" "$arm" "$tier" || echo "######## $label ABORTED rc=$?"
done
echo "######## legs done $(date +%H:%M:%S)"
