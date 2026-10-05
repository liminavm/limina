#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Latency-QoS tier 0 on every vCPU (the supervisor default since limina 88b29e8b) as one A/B on
# one host stack and one guest image (point.sh says how the host half is read):
#   o  off   LIMINA_VCPU_LATENCY_QOS= (empty: tier 0 off, the band alone, as before)
#   t  tier0 the default
#
# Order is o0 n0 o1 n1 o2 -- alternating, with the baseline measured at BOTH ENDS, because the
# host's renderer speed drifts for tens of minutes and a one-ended sweep books that drift as the
# treatment's effect (perf/virglrs-vrend-2026-09-17/legs.sh).
#
# An aborted point is logged and the run moves on; read the log for ABORTED, WEDGED and WATCHDOG.
# Usage: perf/tier0-2026-10-05/legs.sh [first-label]   (resume from a label)
set -uo pipefail
cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
D=perf/tier0-2026-10-05
POINTS=(
  "o0-off off"
  "t0-tier0 tier0"
  "o1-off off"
  "t1-tier0 tier0"
  "o2-off off"
)
started=${1:+0}; started=${started:-1}
for p in "${POINTS[@]}"; do
  read -r label arm <<<"$p"
  [ "$started" = 1 ] || { [ "$label" = "$1" ] && started=1 || continue; }
  echo "######## $label $(date +%H:%M:%S)"
  $D/point.sh "$label" "$arm" || echo "######## $label ABORTED rc=$?"
done
echo "######## legs done $(date +%H:%M:%S)"
