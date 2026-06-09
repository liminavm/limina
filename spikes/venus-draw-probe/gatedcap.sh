#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

cd ~/Projects/limina
LOG=/tmp/seated-clean-worker.log
SSH="ssh -o ConnectTimeout=8 -p 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null claude@127.0.0.1"
TAG="$1"; WANT="$2"   # WANT = string to grep for in env (e.g. COGL_DEBUG)
PRE=$(grep -c 'iosurface_id=Some' "$LOG")
# wait for venus desktop frames
for i in $(seq 1 45); do
  N=$(grep -c 'iosurface_id=Some' "$LOG")
  [ "$N" -gt $((PRE+40)) ] && break
  sleep 3
done
sleep 6
# CONFIRM env active before capturing
PID=$($SSH "pgrep -x gnome-shell | head -1")
ENVHIT=$($SSH "sudo cat /proc/$PID/environ 2>/dev/null | tr '\0' '\n' | grep '$WANT'")
echo "shell pid=$PID  envcheck[$WANT]: ${ENVHIT:-MISSING}"
if [ -z "$ENVHIT" ]; then echo "ABORT: $WANT not in shell env, capture would be meaningless"; exit 1; fi
ID=$(grep -oE 'IOSurface [0-9]+' "$LOG" | tail -1 | grep -oE '[0-9]+')
swift spikes/venus-draw-probe/iosdump.swift "$ID" 2>&1 | tail -1
sips -c 130 760 --cropOffset 620 290 /tmp/ios-$ID.png --out /tmp/dock-$TAG.png >/dev/null 2>&1
sips -z 260 1520 /tmp/dock-$TAG.png --out /tmp/dock-${TAG}x.png >/dev/null 2>&1
echo "DONE TAG=$TAG id=$ID full=/tmp/ios-$ID.png dock=/tmp/dock-${TAG}x.png"
