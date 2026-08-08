#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# Drive Space switches at a fullscreen `notch = extend` VM and timestamp them, so the
# `[OVERLAY-GATE]` trace lines can be read against *when the gesture happened*. The question
# this answers is not "what does isOnActiveSpace say" but "when does it say it, relative to
# the ~0.4 s switch animation" — the overlay is destroyed and recreated on that signal, and a
# recreate at the wrong instant is the visible snap (reported 2026-08-08: sliding back in
# animates without the notch strip, then jumps up to claim it once the animation ends).
#
# Run with the VM already fullscreen on the NOTCHED built-in panel and the worker started with
# LIMINA_OVERLAY_TRACE=1. Space switches follow the display holding the focused window, so this
# activates limina first.
#
# Usage: bash space-switch-probe.sh [dwell-seconds]
set -u
DWELL="${1:-3}"

stamp() { printf '[PROBE] +%s %s\n' "$(date +%H:%M:%S.%N | cut -c1-12)" "$1"; }

key() { # key(keycode) — 123 = left arrow, 124 = right arrow
  osascript -e "tell application \"System Events\" to key code $1 using control down"
}

osascript -e 'tell application "System Events" to set frontmost of (first process whose name is "limina") to true'
sleep 1

stamp "baseline: limina fullscreen, expected overlay UP"
sleep "$DWELL"

stamp "ctrl-right #1 (leave our Space) — expect the overlay to stay up through the slide-out"
key 124
sleep "$DWELL"

stamp "ctrl-left #1 (return) — the reported bug: slide-in WITHOUT the notch strip, then a snap"
key 123
sleep "$DWELL"

stamp "ctrl-right #2 then an immediate ctrl-left (interrupt the switch before it resolves)"
key 124
sleep 0.35
key 123
sleep "$DWELL"

stamp "ctrl-right x2 (two Spaces away) then back, one at a time"
key 124
sleep "$DWELL"
key 124
sleep "$DWELL"
key 123
sleep "$DWELL"
key 123
sleep "$DWELL"

stamp "done"
