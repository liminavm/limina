#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# The 2026-10-09 integration pass: one stack, measured three times after a warm-up (point.sh says
# what the stack is and why the warm-up exists). w0 boots warm.raw in place and writes the guest
# caches under the pass's key; its rows are recorded but are not measurements of a warm stack.
# p1..p3 each boot a clone of the warmed image.
#
# An aborted point is logged and the run moves on; read the log for ABORTED, WEDGED and WATCHDOG.
# Usage: perf/integration-2026-10-09/legs.sh [first-label]   (resume from a label)
set -uo pipefail
cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
D=perf/integration-2026-10-09
POINTS=(
  "w0-warm warm"
  "p1 point"
  "p2 point"
  "p3 point"
)
started=${1:+0}; started=${started:-1}
for p in "${POINTS[@]}"; do
  read -r label arm <<<"$p"
  [ "$started" = 1 ] || { [ "$label" = "$1" ] && started=1 || continue; }
  echo "######## $label $(date +%H:%M:%S)"
  $D/point.sh "$label" "$arm" || echo "######## $label ABORTED rc=$?"
done
echo "######## legs done $(date +%H:%M:%S)"
