#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# The 2026-11-22 trend battery (point.sh says what each point runs). The tiers alternate, three
# boots each, so host drift spreads over both rather than landing on whichever ran last. The
# compute half (scripts/perf-compute.sh) follows, on its own boot.
#
# An aborted point is logged and the run moves on; read the log for ABORTED, WEDGED and WATCHDOG.
# Usage: perf/battery-2026-11-22/legs.sh [first-label]   (resume from a label)
set -uo pipefail
cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
D=perf/battery-2026-11-22
POINTS=(
  "m0 metal"
  "v0 vrend"
  "m1 metal"
  "v1 vrend"
  "m2 metal"
  "v2 vrend"
)
started=${1:+0}; started=${started:-1}
for p in "${POINTS[@]}"; do
  read -r label tier <<<"$p"
  [ "$started" = 1 ] || { [ "$label" = "$1" ] && started=1 || continue; }
  echo "######## $label $(date +%H:%M:%S)"
  $D/point.sh "$label" "$tier" || echo "######## $label ABORTED rc=$?"
done
if [ "${SKIP_COMPUTE:-0}" != 1 ]; then
  echo "######## compute $(date +%H:%M:%S)"
  mkdir -p "$D/evidence"
  scripts/perf-compute.sh "battery-2026-11-22" > "$D/evidence/compute.txt" 2>&1 || echo "######## compute ABORTED rc=$?"
fi
echo "######## legs done $(date +%H:%M:%S)"
