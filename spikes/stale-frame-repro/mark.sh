#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Stamp "I just saw it" onto the same clock the worker log runs on, and keep the frame.
#
# The two faults under investigation have no deliberate trigger and no rate of their own, so the
# only way to line a sighting up against the host-side oracles — the present-order "stepped back"
# warnings and the venus overlap probe's per-window report — is for the person who saw it to say
# when. A sighting nobody timestamped cannot be correlated afterwards, and these do not come back
# on demand.
#
#   spikes/stale-frame-repro/mark.sh ghost stale frame on the btm terminal
#
# It also copies whatever `LIMINA_WINDOW_CAPTURE` last wrote (at most a second old) next to the
# mark, because the periodic capture overwrites itself: archiving a frame only when a human says
# something happened costs nothing while nothing is happening, which a background filmstrip of
# 2 MB/s does not.
set -u
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
HERE="$ROOT/spikes/stale-frame-repro"
MARKS="${LIMINA_MARKS:-$HERE/marks.txt}"
CAPTURE="${LIMINA_WINDOW_CAPTURE:-$HERE/capture.png}"
STAMP=$(date '+%Y-%m-%d_%H-%M-%S')
FRAME=""
if [ -f "$CAPTURE" ]; then
  mkdir -p "$HERE/frames"
  FRAME="frames/$STAMP.png"
  cp "$CAPTURE" "$HERE/$FRAME"
fi
printf '%s  %s%s\n' "$(date '+%Y-%m-%d %H:%M:%S')" "${*:-(no note)}" \
  "${FRAME:+  [$FRAME]}" >> "$MARKS"
tail -1 "$MARKS"
