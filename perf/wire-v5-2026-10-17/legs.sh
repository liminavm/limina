#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# The wire VERSION 5 pass as one A/B of two whole Metal-transport stacks (point.sh says what the
# arms are):
#   v4  the transport before a host queue per command queue, host and guest drivers
#   v5  the transport after it
#
# Order is b0 a0 b1 a1 b2 -- alternating, with the baseline measured at BOTH ENDS, because the
# host's renderer speed drifts for tens of minutes and a one-ended sweep books that drift as the
# treatment's effect (perf/virglrs-vrend-2026-09-17/legs.sh).
#
# An aborted point is logged and the run moves on; read the log for ABORTED, WEDGED and WATCHDOG.
# Usage: perf/wire-v5-2026-10-17/legs.sh [first-label]   (resume from a label)
set -uo pipefail
cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
D=perf/wire-v5-2026-10-17
POINTS=(
  "b0-v4 v4"
  "a0-v5 v5"
  "b1-v4 v4"
  "a1-v5 v5"
  "b2-v4 v4"
)
# ENH_IMAGE=1: the enhanced-image pass (point.sh), its own labels, same alternation.
[ "${ENH_IMAGE:-0}" = 1 ] && POINTS=(
  "eb0-v4 v4"
  "ea0-v5 v5"
  "eb1-v4 v4"
  "ea1-v5 v5"
  "eb2-v4 v4"
)
started=${1:+0}; started=${started:-1}
for p in "${POINTS[@]}"; do
  read -r label arm <<<"$p"
  [ "$started" = 1 ] || { [ "$label" = "$1" ] && started=1 || continue; }
  echo "######## $label $(date +%H:%M:%S)"
  $D/point.sh "$label" "$arm" || echo "######## $label ABORTED rc=$?"
done
echo "######## legs done $(date +%H:%M:%S)"
