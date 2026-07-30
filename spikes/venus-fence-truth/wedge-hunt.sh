#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# wedge-hunt — reproduce the §22 fence-leak-on-context-destroy KMS wedge.
#
# Runs IN THE GUEST (rig image, gsrs autologin). Each round: wait for the gsrs
# niri session, start a sustained overview animation, SIGKILL the compositor at
# a random point mid-animation (async scanout keeps a flip with an unsignaled
# IN_FENCE queued for a few ms every frame — the kill must land inside that
# window), then look for the wedge: the next session/GDM never comes back and
# the kernel logs hung tasks in commit_tail/drm_release.
#
# Host-side oracle to watch in parallel (worker log, LIMINA_GPU_TRACE=1):
#   [GPUTRACE] ... outstanding={...}   — a fence id that never leaves the set.
#
# Usage: sudo bash wedge-hunt.sh [rounds] [mode]
#   mode: kill (SIGKILL, default) | quit (niri quit action — the real logout
#         path, what §22 actually hit) | term (SIGTERM)

set -u
rounds="${1:-30}"
mode="${2:-kill}"
GUID="$(id -u gsrs)"
NIRI_BIN=/home/claude/gnome-shell-rs/target/debug/niri

session_up() {
   pgrep -u gsrs -f "$NIRI_BIN" >/dev/null
}

wait_session() {
   for _ in $(seq 60); do
      session_up && return 0
      sleep 2
   done
   return 1
}

niri_msg() {
   local sock
   sock="$(ls /run/user/$GUID/ 2>/dev/null | grep '^niri\.' | head -1)" || return 1
   [ -n "$sock" ] || return 1
   runuser -u gsrs -- env XDG_RUNTIME_DIR=/run/user/$GUID \
      NIRI_SOCKET=/run/user/$GUID/$sock "$NIRI_BIN" msg "$@"
}

wedged() {
   # The §22 signature: hung kernel tasks in the atomic-commit path.
   dmesg 2>/dev/null | tail -50 | grep -qE "blocked for more than|hung_task" &&
      dmesg 2>/dev/null | tail -80 | grep -qE "commit_tail|drm_release|drm_atomic"
}

echo "wedge-hunt: $rounds rounds"
for round in $(seq "$rounds"); do
   if ! wait_session; then
      if wedged; then
         echo "WEDGED (session never returned) after round $((round - 1))"
         dmesg | grep -B2 -A12 "blocked for more than" | tail -40
         exit 2
      fi
      echo "round $round: session missing but no wedge signature; GDM stuck?"
      loginctl list-sessions --no-legend
      exit 3
   fi
   sleep 3 # let the session settle
   # Windows make the animation frames expensive, widening the per-frame
   # unsignaled-IN_FENCE window the exit must land in.
   for _ in 1 2 3 4; do
      niri_msg action spawn -- ptyxis --new-window >/dev/null 2>&1
      sleep 1
   done
   sleep 2
   # Sustained animation: continuous overview toggles (their PHASE2 shape).
   (
      for _ in $(seq 40); do
         niri_msg action toggle-overview >/dev/null 2>&1
         sleep 0.45
      done
   ) &
   driver=$!
   # Kill mid-animation at a random offset within the toggle cadence.
   sleep "$(python3 -c 'import random; print(round(random.uniform(1.0, 8.0), 3))')"
   case "$mode" in
   quit) niri_msg action quit --skip-confirmation >/dev/null 2>&1 ||
      niri_msg action quit >/dev/null 2>&1 ;;
   term) pkill -TERM -u gsrs -f "$NIRI_BIN" ;;
   *) pkill -9 -u gsrs -f "$NIRI_BIN" ;;
   esac
   kill "$driver" 2>/dev/null
   wait "$driver" 2>/dev/null
   echo "round $round: ended compositor ($mode)"
   sleep 5
   if wedged; then
      echo "WEDGED at round $round"
      dmesg | grep -B2 -A12 "blocked for more than" | tail -40
      exit 2
   fi
done
echo "no wedge in $rounds rounds"
exit 0
