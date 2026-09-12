#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# The sweep's follow-up probes, each through point.sh: one commit inside each large step between
# neighbouring points, stock-guest repeats either side of the step where Basemark's scored run
# began stalling, and the baseline again to price the night's host drift. An aborted probe (a
# virglrs rev that does not build against the libkrun paired with it, say) is logged and skipped.
set -uo pipefail
cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
PROBES=(
  # The sweep's last point. d0416c9 needs libkrun to say whether its GL fences drain, and the pin
  # 4c77915 says they do not -- true only on host mesa 351a1b5e, which this sweep holds out. The
  # patch sets the property false, which is what 4c77915's own message asks of a stock mesa.
  "p10-d0416c9 d0416c9066ec34448362d095daa3d8355536b029 4c7791506bd44b84166138aff598d11fb80bf86a 0 perf/trend-d5889ef/libkrun-4c77915-fences-drain.patch"
  # p00 -> p01: aquarium -40%, Basemark Geometry/Canvas down. 728e64c is the one vrend commit.
  "b1-728e64c 728e64c3b849dc3a7dd0d2d27e7ff38bc76de6bc 952143467a49b51a516d2ac590886e3b7c9b9c99 0"
  # p03 -> p04: aquarium +30%. 8dc2589 (a page-flip finishes the surface's contexts) vs the
  # command-path batch after it.
  "b2-8dc2589 8dc2589 7c4ada05bd15cb81ee03119eb0b8237f4e7e2743 0"
  # p02 -> p03: aquarium recovery. Same virglrs as p03 on p02's libkrun, to split the pair.
  "b3-b1460a6-lk9521434 b1460a654c303dd6ffcfe7e0d9902f120c49ba8f 952143467a49b51a516d2ac590886e3b7c9b9c99 0"
  # Basemark's scored run stalls at p03/p04/p06, never at p00-p02: two stock repeats each side.
  "s1-p02 58aa6d6604afc3b967abbb59be98e0d842b284c7 952143467a49b51a516d2ac590886e3b7c9b9c99 1"
  "s2-p03 b1460a654c303dd6ffcfe7e0d9902f120c49ba8f 7c4ada05bd15cb81ee03119eb0b8237f4e7e2743 1"
  "s3-p02 58aa6d6604afc3b967abbb59be98e0d842b284c7 952143467a49b51a516d2ac590886e3b7c9b9c99 1"
  "s4-p03 b1460a654c303dd6ffcfe7e0d9902f120c49ba8f 7c4ada05bd15cb81ee03119eb0b8237f4e7e2743 1"
  # The baseline again, last: how far the host drifted over the night.
  "r0-d5889ef d5889efba5372483e593be9a5a20989e3a4fc63d 952143467a49b51a516d2ac590886e3b7c9b9c99 0"
)
for p in "${PROBES[@]}"; do
  read -r label vrev lrev stock patch <<<"$p"
  echo "######## $label"
  STOCK_ONLY="$stock" LIBKRUN_PATCH="${patch:-}" perf/trend-d5889ef/point.sh "$label" "$vrev" "$lrev" || echo "######## $label ABORTED rc=$?"
done
echo "######## bisect done"
