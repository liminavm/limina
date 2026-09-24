#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# The stock tier's side: the freeworld-VA stock image, playing through Showtime (GNOME's player;
# stock Firefox decodes in software there), at three frame sizes and idle. A stock guest decodes
# into per-plane targets, whose planes upload on the render thread, and virglrs 2efef4f
# (target/lat-stock) times those uploads. Two rounds, alternated, one boot per point.
set -uo pipefail
cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
ARMS="${ARMS:-idle 720 1080 2160}"
res() { # <arm> -> resolution; a case, since macOS bash 3.2 has no maps
  case "$1" in
    720) echo 1280x720 ;; 1080) echo 1920x1080 ;; 2160) echo 3840x2160 ;; idle) echo 1280x720 ;;
  esac
}
for round in 1 2; do
  for arm in $ARMS; do
    L="stock-$arm-r$round"
    mode=video; [ "$arm" = idle ] && mode=idle
    echo "######## $L $(date +%H:%M:%S)"
    IMAGE=Fedora-Workstation-44.stock.test.raw PLAYER=showtime CLIP_RES="$(res "$arm")" \
      spikes/flush-latency/point.sh "$L" target/lat-stock 0 "$mode" || echo "######## $L ABORTED rc=$?"
  done
done
echo "######## done $(date +%H:%M:%S)"
