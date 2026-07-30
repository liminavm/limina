#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# measure-arm — one present-path measurement arm on the gsrs rig (task #18 A/B).
# Runs IN THE GUEST as root. Waits for the gsrs session, spawns 4 windows,
# runs drive-workload mixed then heavy, and dumps the session journal
# (NIRI_FRAME_LOG=all,gpu lines included) to /tmp/arm-<label>.log.
#
# Usage: sudo bash measure-arm.sh <label>

set -eu
label="${1:?label}"
GUID="$(id -u gsrs)"
NIRI_BIN=/home/claude/gnome-shell-rs/target/debug/niri
REPO=/home/claude/gnome-shell-rs

for _ in $(seq 60); do
   pgrep -u gsrs -f "$NIRI_BIN" >/dev/null && break
   sleep 2
done
pgrep -u gsrs -f "$NIRI_BIN" >/dev/null || { echo "no session"; exit 1; }
sleep 5

sock="$(ls /run/user/$GUID/ | grep '^niri\.' | head -1)"
N() {
   runuser -u gsrs -- env XDG_RUNTIME_DIR=/run/user/$GUID \
      NIRI_SOCKET=/run/user/$GUID/$sock "$NIRI_BIN" "$@"
}
# Frame-log env must be live in the compositor (environment.d needs the session
# to have started after the file landed) — verify, don't assume.
if ! sudo cat /proc/"$(pgrep -u gsrs -f "$NIRI_BIN" | head -1)"/environ |
   tr '\0' '\n' | grep -q "NIRI_FRAME_LOG=all,gpu"; then
   echo "NIRI_FRAME_LOG not live in the compositor env — relogin needed"
   exit 4
fi

for _ in 1 2 3 4; do
   N msg action spawn -- "${TERM_APP:-ptyxis}" ${TERM_APP_ARGS---new-window} >/dev/null 2>&1
   sleep 1.5
done
sleep 4
wins="$(N msg --json windows | python3 -c 'import json,sys;print(len(json.load(sys.stdin)))')"
echo "windows=$wins"
[ "$wins" -ge 4 ] || { echo "window spawn failed"; exit 5; }

# HOG=1: run a concurrent GPU load (drawstorm) so compositor frames land in the
# dogfood's gpu-p50 band (their 5.5-12 ms vs the rig's idle ~2 ms) — GPU
# contention inflates niri's render time without touching its workload.
hog_pid=""
if [ "${HOG:-0}" = "1" ]; then
   [ -x /home/claude/drawstorm ] || { echo "no drawstorm binary"; exit 6; }
   for _ in $(seq "${HOG_JOBS:-1}"); do
      runuser -u gsrs -- env VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json \
         bash -c 'while true; do /home/claude/drawstorm -n 10000 -f 400 -i 3 >/dev/null 2>&1; done' &
      hog_pid="$hog_pid $!"
   done
   echo "gpu hog running (${HOG_JOBS:-1} jobs: $hog_pid)"
   sleep 3
fi

start="$(date '+%Y-%m-%d %H:%M:%S')"
echo "ARM $label START $start"
bash "$REPO/scripts/drive-workload.sh" "$GUID" 1 mixed
bash "$REPO/scripts/drive-workload.sh" "$GUID" 1 heavy
if [ -n "$hog_pid" ]; then
   kill $hog_pid 2>/dev/null || true
   pkill -u gsrs -f drawstorm 2>/dev/null || true
fi
sleep 3
journalctl _UID="$GUID" --since "$start" |
   sed -r 's/\x1B\[[0-9;]*[mGKHJ]//g' > "/tmp/arm-$label.log"
echo "ARM $label DONE: $(grep -c 'missed .* vblank' "/tmp/arm-$label.log" || true) miss lines, $(grep -c 'fps over' "/tmp/arm-$label.log" || true) summaries -> /tmp/arm-$label.log"
