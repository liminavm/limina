#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# The GPU-transport pass as one A/B on one host stack and one guest image (point.sh says what the
# arms are):
#   v  venus  the venus ICD, GL through zink on venus
#   m  metal  the native-context transport's KosmicKrisp ICD, GL through zink on it
#
# Order is v0 m0 v1 m1 v2 -- alternating, with the baseline measured at BOTH ENDS, because the
# host's renderer speed drifts for tens of minutes and a one-ended sweep books that drift as the
# treatment's effect (perf/virglrs-vrend-2026-09-17/legs.sh).
#
# An aborted point is logged and the run moves on; read the log for ABORTED, WEDGED and WATCHDOG.
# Usage: perf/macos27-2026-10-15/legs.sh [first-label]   (resume from a label)
set -uo pipefail
cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
D=perf/macos27-2026-10-15
POINTS=(
  "v0-venus venus"
  "m0-metal metal"
  "v1-venus venus"
  "m1-metal metal"
  "v2-venus venus"
)
# AQ_ONLY=1: the aquarium-only pass, its own labels, same alternation.
[ "${AQ_ONLY:-0}" = 1 ] && POINTS=(
  "av0-venus venus"
  "am0-metal metal"
  "av1-venus venus"
  "am1-metal metal"
  "av2-venus venus"
)
# EXTRA=1: the enhanced-image arm, bracketed by the metal arm on the image the main sweep uses.
[ "${EXTRA:-0}" = 1 ] && POINTS=(
  "e0-enhanced enhanced"
  "m2-metal metal"
  "e1-enhanced enhanced"
)
started=${1:+0}; started=${started:-1}
for p in "${POINTS[@]}"; do
  read -r label arm <<<"$p"
  [ "$started" = 1 ] || { [ "$label" = "$1" ] && started=1 || continue; }
  echo "######## $label $(date +%H:%M:%S)"
  $D/point.sh "$label" "$arm" || echo "######## $label ABORTED rc=$?"
done
echo "######## legs done $(date +%H:%M:%S)"
