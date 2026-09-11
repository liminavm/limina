#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Second round of the sweep's probes, run after bisect.sh.
#  - Basemark WebGL 2.0 fell ~14% (4674 -> 4038) and Canvas rose back to baseline (981 -> 1174) between
#    p05 (feef548) and p07 (4654a34); p06 would place both, but its scored run stalled. Two stock repeats.
#  - The p03 -> p04 aquarium gain is in the command-path batch after 7c65f0f (b2 read like p03): one
#    midpoint probe at 00657a4 halves it.
set -uo pipefail
cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
PROBES=(
  "s5-p06 4bfdefe89dbbfa3b21c6df07d479f7c5e41c1f26 0dba6b9c4029bd75e5f668ffb33942c39061e95d 1"
  "s6-p06 4bfdefe89dbbfa3b21c6df07d479f7c5e41c1f26 0dba6b9c4029bd75e5f668ffb33942c39061e95d 1"
  "b4-00657a4 00657a4 7c4ada05bd15cb81ee03119eb0b8237f4e7e2743 0"
  # s4-p03 in bisect.sh never measured (the site printed no configuration); p03 needs one more sample.
  "s7-p03 4afa3ef2974723b918004699479b5c6b9a239002 7c4ada05bd15cb81ee03119eb0b8237f4e7e2743 1"
)
for p in "${PROBES[@]}"; do
  read -r label vrev lrev stock patch <<<"$p"
  echo "######## $label"
  STOCK_ONLY="$stock" LIBKRUN_PATCH="${patch:-}" perf/trend-0a9cf16/point.sh "$label" "$vrev" "$lrev" || echo "######## $label ABORTED rc=$?"
done
echo "######## bisect2 done"
