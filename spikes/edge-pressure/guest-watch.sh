#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# The guest half of the edge-pressure oracle: does the forwarded edge pressure arrive on the
# relative device, and does mutter act on it?
#
#   bash guest-watch.sh <ssh-port> [seconds]
#
# Prints, once per poll, GNOME's own OverviewActive property — the authoritative answer to "did
# the hot corner fire", with no human eyeballing a screen — and, at the end, the relative-motion
# events the guest actually received during the window.
set -uo pipefail
PORT="${1:?usage: guest-watch.sh <ssh-port> [seconds]}"
SECS="${2:-6}"
SSH=(ssh -p "$PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null
     -o BatchMode=yes -o ConnectTimeout=8 claude@127.0.0.1)

# The relative device is the one edge pressure is forwarded to; the absolute tablet carries the
# ordinary cursor. Resolve by name so a device-order change can't silently watch the wrong one.
DEV=$("${SSH[@]}" 'for e in /sys/class/input/event*; do
        n=$(cat "$e/device/name" 2>/dev/null)
        [ "$n" = "limina Virtual Mouse" ] && echo "/dev/input/${e##*/}"
      done' | head -1)
echo "relative device: ${DEV:-NOT FOUND}"

"${SSH[@]}" "sudo timeout $SECS cat $DEV | xxd -c 16 | head -40" > /tmp/edge-guest-events.txt 2>&1 &
EVPID=$!

for _ in $(seq 1 "$((SECS * 4))"); do
  printf '%s ' "$("${SSH[@]}" 'export XDG_RUNTIME_DIR=/run/user/$(id -u);
      busctl --user get-property org.gnome.Shell /org/gnome/Shell org.gnome.Shell OverviewActive' 2>/dev/null)"
  sleep 0.25
done
echo
wait $EVPID 2>/dev/null
echo "--- raw relative-device traffic (empty = the guest never saw the pressure):"
cat /tmp/edge-guest-events.txt
