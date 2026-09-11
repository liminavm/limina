#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Run the trend sweep's points in order, each through point.sh. An aborted point is
# logged and the sweep moves on; read the log for ABORTED, WEDGED and WATCHDOG.
# Usage: perf/trend-0a9cf16/sweep.sh [first-label]   (resume from a label)
set -uo pipefail
cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
POINTS=(
  "p01-58a29c9 58a29c9b1ac982cbde7f76a90df3bc8a6b809b9b 952143467a49b51a516d2ac590886e3b7c9b9c99"
  "p02-42008bb 42008bb6a231613667a80dbf2d207ec38120077e 952143467a49b51a516d2ac590886e3b7c9b9c99"
  "p03-4afa3ef 4afa3ef2974723b918004699479b5c6b9a239002 7c4ada05bd15cb81ee03119eb0b8237f4e7e2743"
  "p04-d30b8ce d30b8ce87ec5f3dc8a3974a76cb8c9b9d624e94b 7c4ada05bd15cb81ee03119eb0b8237f4e7e2743"
  "p05-feef548 feef5488deaf3faa1c7a84d0aee7ec1d0d81a9ff 7c4ada05bd15cb81ee03119eb0b8237f4e7e2743"
  "p06-4bfdefe 4bfdefe89dbbfa3b21c6df07d479f7c5e41c1f26 0dba6b9c4029bd75e5f668ffb33942c39061e95d"
  "p07-4654a34 4654a345faab18d908736f962a1aa1ac8b655a36 bae5de4ab86ebd4087468fdefe4ee105099ad397"
  "p08-593350e 593350ecf230ccdab11db9f67cf3d6ab6b5eaaf3 bae5de4ab86ebd4087468fdefe4ee105099ad397"
  "p09-18fc8df 18fc8df bae5de4ab86ebd4087468fdefe4ee105099ad397"
  # libkrun 4c779150 (the pin paired with d0416c9) needs host mesa 351a1b5e, which this sweep holds out.
  "p10-d0416c9 d0416c9066ec34448362d095daa3d8355536b029 bae5de4ab86ebd4087468fdefe4ee105099ad397"
)
started=${1:+0}; started=${started:-1}
for p in "${POINTS[@]}"; do
  read -r label vrev lrev <<<"$p"
  [ "$started" = 1 ] || { [ "$label" = "$1" ] && started=1 || continue; }
  echo "######## $label"
  # An aborted point is logged and skipped: one wedged guest must not strand the rest overnight.
  perf/trend-0a9cf16/point.sh "$label" "$vrev" "$lrev" || echo "######## $label ABORTED rc=$?"
done
echo "######## sweep done"
