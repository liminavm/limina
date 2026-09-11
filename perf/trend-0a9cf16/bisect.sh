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
  "p10-d0416c9 d0416c9066ec34448362d095daa3d8355536b029 4c7791506bd44b84166138aff598d11fb80bf86a 0 perf/trend-0a9cf16/libkrun-4c77915-fences-drain.patch"
  # p00 -> p01: aquarium -40%, Basemark Geometry/Canvas down. 894b36d is the one vrend commit.
  "b1-894b36d 894b36d9270e03ef176c530556c256eae4c39f0f 952143467a49b51a516d2ac590886e3b7c9b9c99 0"
  # p03 -> p04: aquarium +30%. 7c65f0f (a page-flip finishes the surface's contexts) vs the
  # command-path batch after it.
  "b2-7c65f0f 7c65f0f 7c4ada05bd15cb81ee03119eb0b8237f4e7e2743 0"
  # p02 -> p03: aquarium recovery. Same virglrs as p03 on p02's libkrun, to split the pair.
  "b3-4afa3ef-lk9521434 4afa3ef2974723b918004699479b5c6b9a239002 952143467a49b51a516d2ac590886e3b7c9b9c99 0"
  # Basemark's scored run stalls at p03/p04/p06, never at p00-p02: two stock repeats each side.
  "s1-p02 42008bb6a231613667a80dbf2d207ec38120077e 952143467a49b51a516d2ac590886e3b7c9b9c99 1"
  "s2-p03 4afa3ef2974723b918004699479b5c6b9a239002 7c4ada05bd15cb81ee03119eb0b8237f4e7e2743 1"
  "s3-p02 42008bb6a231613667a80dbf2d207ec38120077e 952143467a49b51a516d2ac590886e3b7c9b9c99 1"
  "s4-p03 4afa3ef2974723b918004699479b5c6b9a239002 7c4ada05bd15cb81ee03119eb0b8237f4e7e2743 1"
  # The baseline again, last: how far the host drifted over the night.
  "r0-0a9cf16 0a9cf162c62f3a127e91627966dc63b2edef2512 952143467a49b51a516d2ac590886e3b7c9b9c99 0"
)
for p in "${PROBES[@]}"; do
  read -r label vrev lrev stock patch <<<"$p"
  echo "######## $label"
  STOCK_ONLY="$stock" LIBKRUN_PATCH="${patch:-}" perf/trend-0a9cf16/point.sh "$label" "$vrev" "$lrev" || echo "######## $label ABORTED rc=$?"
done
echo "######## bisect done"
