#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# Change-driven timeline of the display chain, for an interactive EDID/monitor repro
# session. Polls the four links (see sample.sh for what each one proves) and appends a
# timestamped block to the timeline ONLY when something actually changed — so the file
# reads as "what happened, in order" rather than as a wall of identical polls. The user
# demonstrates a problem at a moment we cannot predict; this is the scrollback.
#
# Usage: bash watch.sh <ssh-port> <out-dir> [interval-seconds]
set -u
PORT="${1:?usage: watch.sh <ssh-port> <out-dir> [interval]}"
OUT="${2:?usage: watch.sh <ssh-port> <out-dir> [interval]}"
IVL="${3:-6}"
LOG="${LIMINA_WORKER_LOG:-/tmp/enhanced-efi-kk-worker.log}"
STATE_TOML="$(dirname "$0")/edid-repro.state.toml"
mkdir -p "$OUT"
TL="$OUT/timeline.log"

SSH=(ssh -p "$PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null
     -o LogLevel=ERROR -o BatchMode=yes -o ConnectTimeout=8
     -o ControlMaster=auto -o ControlPath=/tmp/limina-dispmon-%p -o ControlPersist=120
     claude@127.0.0.1)

# One ssh round-trip gathers every guest-side fact; keeping it to one connection is what
# makes a 6-second poll cheap enough to leave running for a whole session.
GUEST_PROBE='
export XDG_RUNTIME_DIR=/run/user/$(id -u)
export DBUS_SESSION_BUS_ADDRESS=unix:path=$XDG_RUNTIME_DIR/bus
for c in /sys/class/drm/card*-*/; do
  n=$(basename "$c")
  echo "DRM $n $(cat "$c/status" 2>/dev/null) $(cat "$c/enabled" 2>/dev/null) modes=[$(tr "\n" " " < "$c/modes" 2>/dev/null)]"
  echo "EDID $n $(base64 -w0 < "$c/edid" 2>/dev/null | md5sum | cut -c1-12) $(base64 -w0 < "$c/edid" 2>/dev/null | wc -c)"
done
echo "MUTTER $(gdbus call --session -d org.gnome.Mutter.DisplayConfig -o /org/gnome/Mutter/DisplayConfig -m org.gnome.Mutter.DisplayConfig.GetCurrentState 2>&1)"
echo "MONXML $(md5sum ~/.config/monitors.xml 2>/dev/null | cut -c1-12) $(stat -c %y ~/.config/monitors.xml 2>/dev/null)"
'

prev=""
last_heartbeat=0
echo "=== watch started $(date -u +%Y-%m-%dT%H:%M:%SZ) port=$PORT interval=${IVL}s ===" >>"$TL"
while true; do
  now=$(date -u +%Y-%m-%dT%H:%M:%SZ)

  win=$(osascript -e 'tell application "System Events" to tell (first process whose name is "limina") to get {position, size} of window 1' 2>/dev/null)
  st=$(tr -d ' \n' < "$STATE_TOML" 2>/dev/null)
  push=$(grep -a "display-control: pushed" "$LOG" 2>/dev/null | tail -1)
  npush=$(grep -ac "display-control: pushed" "$LOG" 2>/dev/null)
  guest=$("${SSH[@]}" "$GUEST_PROBE" 2>&1)

  cur="WINDOW $win
STATE $st
PUSHES $npush | $push
$guest"

  if [ "$cur" != "$prev" ]; then
    {
      echo
      echo "### $now  (change)"
      if [ -n "$prev" ]; then
        # A unified diff of the previous sample makes the actual event obvious at a glance;
        # the full state follows so the file stays self-contained when read out of order.
        diff <(printf '%s\n' "$prev") <(printf '%s\n' "$cur") | sed 's/^/  ~ /'
        echo "  --- full state ---"
      fi
      printf '%s\n' "$cur" | sed 's/^/  /'
    } >>"$TL"
    prev="$cur"
    last_heartbeat=$(date +%s)
  else
    n=$(date +%s)
    if [ $((n - last_heartbeat)) -ge 120 ]; then
      echo "### $now  (unchanged heartbeat)" >>"$TL"
      last_heartbeat=$n
    fi
  fi
  sleep "$IVL"
done
