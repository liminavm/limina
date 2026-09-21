#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Record the presented scanout as a sequence of distinct frames, so frames can be compared
# against each other after the fact.
#
#   spikes/stale-frame-repro/filmstrip.sh [seconds]
#
# The fault under investigation is a client's content going BACKWARDS -- frame N, then N+1, then
# N again -- which no single frame can show and no human can hold in their head at one frame a
# second. Comparing frames to each other is the whole measurement, so the frames have to be kept
# side by side rather than overwritten.
#
# LIMINA_WINDOW_CAPTURE rewrites one file at its own cadence, so this polls faster than that and
# keeps a frame only when the content actually changed. That way the strip holds each distinct
# presented frame exactly once, and an identical pair in the strip means the capture really
# repeated rather than that the poller sampled twice between writes.
set -u
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
HERE="$ROOT/spikes/stale-frame-repro"
CAPTURE="${LIMINA_WINDOW_CAPTURE:-$HERE/capture.png}"
SECS="${1:-40}"
STRIP="$HERE/strip-$(date '+%H-%M-%S')"
mkdir -p "$STRIP"

echo "recording $SECS s to $STRIP"
LAST=""
N=0
END=$(( $(date +%s) + SECS ))
while [ "$(date +%s)" -lt "$END" ]; do
  # The worker rewrites the capture in place, so a poll can land mid-write and read a truncated
  # or empty file. Those read as "a new frame" and then as "a repeat" of each other, which is
  # exactly the finding this script exists to report -- a torn read must never be able to
  # manufacture the fault.
  #
  # Testing the live file and then hashing it is not enough, and the first version of this did
  # exactly that: the file can be truncated between the test and the hash, so empty reads still
  # got through and reported themselves as the content repeating. Take a private copy first and
  # judge the copy, which nothing else can change underneath.
  cp "$CAPTURE" "$STRIP/.candidate" 2>/dev/null || { sleep 0.2; continue; }
  if [ -s "$STRIP/.candidate" ] && \
     [ "$(tail -c 8 "$STRIP/.candidate" | xxd -p)" = "49454e44ae426082" ]; then
    SUM=$(shasum -a 256 "$STRIP/.candidate" | cut -d' ' -f1)
    if [ "$SUM" != "$LAST" ]; then
      N=$((N + 1))
      cp "$STRIP/.candidate" "$(printf '%s/%03d.png' "$STRIP" "$N")"
      printf '%s  %03d  %s\n' "$(date '+%H:%M:%S')" "$N" "${SUM:0:12}" >> "$STRIP/frames.txt"
      LAST="$SUM"
    fi
  fi
  sleep 0.2
done
echo "$N distinct frames"
echo "--- exact repeats (a frame identical to an EARLIER one, which is content going backwards) ---"
sort -k3 "$STRIP/frames.txt" | awk '{ if ($3 == prev) print "  " prevline "  ==  " $0; prev = $3; prevline = $0 }'
