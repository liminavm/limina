#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# The chrome ask under the STRIP overlay: does the gesture still fire?
#
# Reported 2026-08-08: fullscreen, `notch = extend`, lean at the top edge — the macOS titlebar
# and its traffic lights appear (they land BELOW the band and are usable), but the menu bar
# stays covered, i.e. the strip never came down.
#
# That symptom has exactly two shapes and they are indistinguishable by eye:
#   (a) the ask was never granted. macOS reveals its own chrome on hover regardless of us; the
#       titlebar showing proves the SYSTEM revealed, not that WE were told. `reveal_chrome`
#       stays false, `claims_band` stays true, the strip stays up over the menu bar.
#   (b) the ask WAS granted and `hide` failed to take the strip off screen.
#
# `[OVERLAY-GATE] ... reveal_chrome=` separates them in one line, and `[REVEAL]` says why the
# gesture ended if it is (a) — every call is traced with the reason it returned.
#
# The gesture is a sustained physical lean; synthetic warps do not reproduce it (a warp opens a
# 0.25 s suppression window — see the `limina-edge-resistance` memory). A human drives this.
#
# Usage: LIMINA_DISK=<image.raw> bash spikes/notch-fullscreen/chrome-ask-probe.sh
set -u
cd "$(dirname "$0")/../.."
# Where boot-enhanced-efi-kk.sh puts the run's stderr — supervisor traces land there too.
LOG=/tmp/enhanced-efi-kk-worker.log

export LIMINA_OVERLAY_TRACE=1 LIMINA_EDGE_TRACE=1 LIMINA_DISPLAY_TRACE=1
# `--notch` defaults to `avoid`, and under `avoid` there is no band to stand down: the run looks
# healthy and answers nothing. Cost one round trip on 2026-08-08 — `claims_band=false` with a
# perfectly good `notch=32 learned=33` beside it is the tell.
export LIMINA_EXTRA_ARGS="--notch extend ${LIMINA_EXTRA_ARGS:-}"

if [ "${1:-}" = "--read" ]; then
  echo "=== OVERLAY-GATE (reveal_chrome=? / up=?) ==="
  grep -h '\[OVERLAY-GATE\]' "$LOG" || echo "(none)"
  echo "=== STRIP ==="
  grep -h '\[STRIP\]' "$LOG" || echo "(none)"
  echo "=== REVEAL, last 60 (why the gesture ended) ==="
  grep -h '\[REVEAL\]' "$LOG" | tail -60 || echo "(none — the gesture never reached reveal_step)"
  exit 0
fi

cat <<EOF
Log: $LOG
Once the desktop is up:
  1. Cmd-Ctrl-F to go fullscreen on the BUILT-IN (notched) panel.
  2. Lean the pointer up against the very top edge and hold ~1 s.
  3. Report whether the menu bar became visible.
Then: bash spikes/notch-fullscreen/chrome-ask-probe.sh --read
EOF

exec spikes/venus-draw-probe/boot-enhanced-efi-kk.sh
