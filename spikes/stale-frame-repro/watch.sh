#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# One keypress marks a sighting. Run it in a host terminal and leave it running.
#
#   s  a stale frame / duplicated rows in a client's content
#   b  the square border around a window
#   t  typing went missing and came back
#   o  something else worth a look
#   q  quit
#
# Typing a note takes seconds, and by the time it is written the thing that prompted it has
# usually stopped -- so the note ends up describing the wrong moment, or the person stops
# reporting. A single key costs nothing, which is the only cost a human will pay repeatedly while
# they are busy watching for something else.
set -u
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
HERE="$ROOT/spikes/stale-frame-repro"

label() {
  case "$1" in
    s) echo "STALE   stale frame / duplicated rows" ;;
    b) echo "BORDER  square border around a window" ;;
    t) echo "TYPING  typed characters went missing" ;;
    o) echo "OTHER   unclassified" ;;
    *) echo "" ;;
  esac
}

echo "marking to $HERE/marks.txt -- s stale, b border, t typing, o other, q quit"
while IFS= read -rsn1 key; do
  [ "$key" = "q" ] && { echo "done"; break; }
  what=$(label "$key")
  [ -z "$what" ] && { echo "  (ignored '$key' -- s/b/t/o/q)"; continue; }
  "$HERE/mark.sh" "$what"
done
