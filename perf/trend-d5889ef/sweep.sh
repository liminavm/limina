#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Run the trend sweep's points in order, each through point.sh. An aborted point is
# logged and the sweep moves on; read the log for ABORTED, WEDGED and WATCHDOG.
# Usage: perf/trend-d5889ef/sweep.sh [first-label]   (resume from a label)
set -uo pipefail
cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
POINTS=(
  "p01-0ed4eb7 0ed4eb7639996b57bd6a0813b94edce38ab7298e 952143467a49b51a516d2ac590886e3b7c9b9c99"
  "p02-58aa6d6 58aa6d6604afc3b967abbb59be98e0d842b284c7 952143467a49b51a516d2ac590886e3b7c9b9c99"
  "p03-b1460a6 b1460a654c303dd6ffcfe7e0d9902f120c49ba8f 7c4ada05bd15cb81ee03119eb0b8237f4e7e2743"
  "p04-4813d7a 4813d7a1e6ff43113e07bc5a6dd36f751d1e404e 7c4ada05bd15cb81ee03119eb0b8237f4e7e2743"
  "p05-c6b0d21 c6b0d21c3ddb08992e84e0d68d6c075f1d2b6760 7c4ada05bd15cb81ee03119eb0b8237f4e7e2743"
  "p06-24a91eb 24a91ebc84db07b184e678ea8df5db7d8db16ad2 0dba6b9c4029bd75e5f668ffb33942c39061e95d"
  "p07-02d047b 02d047be22d96737b087be49a12ab89c80e12e58 bae5de4ab86ebd4087468fdefe4ee105099ad397"
  "p08-94572f5 94572f596885bd7f5923976634b135693ebb5399 bae5de4ab86ebd4087468fdefe4ee105099ad397"
  "p09-d27a693 d27a693 bae5de4ab86ebd4087468fdefe4ee105099ad397"
  # libkrun 4c779150 (the pin paired with d0416c9) needs host mesa 351a1b5e, which this sweep holds out.
  "p10-d0416c9 d0416c9066ec34448362d095daa3d8355536b029 bae5de4ab86ebd4087468fdefe4ee105099ad397"
)
started=${1:+0}; started=${started:-1}
for p in "${POINTS[@]}"; do
  read -r label vrev lrev <<<"$p"
  [ "$started" = 1 ] || { [ "$label" = "$1" ] && started=1 || continue; }
  echo "######## $label"
  # An aborted point is logged and skipped: one wedged guest must not strand the rest overnight.
  perf/trend-d5889ef/point.sh "$label" "$vrev" "$lrev" || echo "######## $label ABORTED rc=$?"
done
echo "######## sweep done"
