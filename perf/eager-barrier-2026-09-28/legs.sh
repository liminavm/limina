#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Score host zink's eager end-of-pass barrier (limina-kk d84192b397e) as one A/B on one build:
#   b  LIMINA_ZINK_NO_EAGER_RP_BARRIER=1   the barrier off, the baseline
#   n  as shipped                          the barrier on
#
# Order is b0 n0 b1 n1 b2 -- alternating, with the baseline measured at BOTH ENDS, because the
# host's renderer speed drifts for tens of minutes and a one-ended sweep books that drift as the
# treatment's effect (perf/virglrs-vrend-2026-09-17/legs.sh).
#
# An aborted point is logged and the run moves on; read the log for ABORTED, WEDGED and WATCHDOG.
# Usage: perf/eager-barrier-2026-09-28/legs.sh [first-label]   (resume from a label)
set -uo pipefail
cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
D=perf/eager-barrier-2026-09-28
POINTS=(
  "b0-off off"
  "n0-on on"
  "b1-off off"
  "n1-on on"
  "b2-off off"
)
started=${1:+0}; started=${started:-1}
for p in "${POINTS[@]}"; do
  read -r label arm <<<"$p"
  [ "$started" = 1 ] || { [ "$label" = "$1" ] && started=1 || continue; }
  echo "######## $label $(date +%H:%M:%S)"
  $D/point.sh "$label" "$arm" || echo "######## $label ABORTED rc=$?"
done
echo "######## legs done $(date +%H:%M:%S)"
