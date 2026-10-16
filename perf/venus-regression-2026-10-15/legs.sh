#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# The venus-regression A/B: the last September baseline bundle (a, 2026-09-10-8 = limina
# 6b0af433) against today's stack (b, 2026-10-15-1), alternated a b a b a so the baseline is
# measured at both ends, on one image. Before each point it waits for the host to be quiet: no
# other process over QUIET_PCT CPU for two consecutive samples a minute apart. WindowServer is
# exempt: it idles near 40% on this desktop, and every point's window pays it alike.
#
# Usage: perf/venus-regression-2026-10-15/legs.sh <a Limina.app> <b Limina.app> [first-label]
set -uo pipefail
A="${1:?baseline Limina.app}"; B="${2:?candidate Limina.app}"
cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
D=perf/venus-regression-2026-10-15
QUIET_PCT=${QUIET_PCT:-30}
quiet_gate() {
  local ok=0
  while [ "$ok" -lt 2 ]; do
    local top; top=$(ps -Ao pcpu,comm -r | awk -v t="$QUIET_PCT" 'NR>1 && $1>t && $2 !~ /WindowServer$/ {print $1, $2}')
    if [ -z "$top" ]; then ok=$((ok + 1)); else ok=0; echo "[$(date +%H:%M:%S)] host busy: $(echo "$top" | tr '\n' ';' | cut -c1-200)"; fi
    [ "$ok" -lt 2 ] && sleep 60
  done
}
POINTS=("a0 $A" "b0 $B" "a1 $A" "b1 $B" "a2 $A")
started=${3:+0}; started=${started:-1}
for p in "${POINTS[@]}"; do
  read -r label app <<<"$p"
  [ "$started" = 1 ] || { [ "$label" = "$3" ] && started=1 || continue; }
  quiet_gate
  echo "######## $label $(date +%H:%M:%S)"
  $D/point.sh "$label" "$app" || echo "######## $label ABORTED rc=$?"
done
echo "######## legs done $(date +%H:%M:%S)"
