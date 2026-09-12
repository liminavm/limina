#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Second round of the sweep's probes, run after bisect.sh.
#  - Basemark WebGL 2.0 fell ~14% (4674 -> 4038) and Canvas rose back to baseline (981 -> 1174) between
#    p05 (c6b0d21) and p07 (02d047b); p06 would place both, but its scored run stalled. Two stock repeats.
#  - The p03 -> p04 aquarium gain is in the command-path batch after 8dc2589 (b2 read like p03): one
#    midpoint probe at 141bf4d halves it.
set -uo pipefail
cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
PROBES=(
  "s5-p06 24a91ebc84db07b184e678ea8df5db7d8db16ad2 0dba6b9c4029bd75e5f668ffb33942c39061e95d 1"
  "s6-p06 24a91ebc84db07b184e678ea8df5db7d8db16ad2 0dba6b9c4029bd75e5f668ffb33942c39061e95d 1"
  "b4-141bf4d 141bf4d 7c4ada05bd15cb81ee03119eb0b8237f4e7e2743 0"
  # s4-p03 in bisect.sh never measured (the site printed no configuration); p03 needs one more sample.
  "s7-p03 b1460a654c303dd6ffcfe7e0d9902f120c49ba8f 7c4ada05bd15cb81ee03119eb0b8237f4e7e2743 1"
)
for p in "${PROBES[@]}"; do
  read -r label vrev lrev stock patch <<<"$p"
  echo "######## $label"
  STOCK_ONLY="$stock" LIBKRUN_PATCH="${patch:-}" perf/trend-d5889ef/point.sh "$label" "$vrev" "$lrev" || echo "######## $label ABORTED rc=$?"
done
echo "######## bisect2 done"
