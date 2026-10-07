#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Runs a series of point-remote.sh points, each under rss-watch.sh on the remote Mac.
#
# Usage: REMOTE=<user@host> RDIR=<remote dir> [APP=<bundle>] run-points.sh <label>:<arm> ...
#   e.g. run-points.sh ps0:skip pu0:unroll ps1:skip
#
# The watcher is the only thing between the unroll arm and a hard-hung host, so a point does not
# start unless its watcher is running and has written its file, and the series stops at the first
# point whose watcher had to kill the VM: a guard that fired means the host is in trouble, and the
# next point would start on a host that has not recovered.
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
REMOTE="${REMOTE:?REMOTE=<user@host>}"; RDIR="${RDIR:?RDIR=<remote dir>}"
DISK="${DISK:-ab-enh.raw}"
FP_LIMIT="${FP_LIMIT:-10240}"        # MiB of worker footprint
COMP_GROWTH="${COMP_GROWTH:-2048}"   # MiB of host compressor growth during the point
MAX_S="${MAX_S:-1500}"
SSHO=(-o BatchMode=yes)

scp "${SSHO[@]}" -q "$HERE/rss-watch.sh" "$REMOTE:$RDIR/" || exit 1

for point in "$@"; do
  label="${point%%:*}"; arm="${point#*:}"
  tsv="rss-$label.tsv"
  ssh "${SSHO[@]}" "$REMOTE" "cd '$RDIR' && rm -f '$tsv' && (nohup bash rss-watch.sh '$DISK' '$tsv' $FP_LIMIT $COMP_GROWTH $MAX_S > /dev/null 2>&1 < /dev/null &) && sleep 2 && pgrep -f '[r]ss-watch.sh $DISK $tsv' > /dev/null && test -s '$tsv'" || {
    echo "$label: watcher did not start; stopping the series" >&2
    exit 1
  }

  "$HERE/point-remote.sh" "$label" "$arm"
  echo "$label rc=$?"
  ssh "${SSHO[@]}" "$REMOTE" "pkill -f '[r]ss-watch.sh $DISK $tsv'" || true

  mkdir -p "$HERE/evidence/$label"
  scp "${SSHO[@]}" -q "$REMOTE:$RDIR/$tsv" "$HERE/evidence/$label/" || true
  grep '^#' "$HERE/evidence/$label/$tsv" 2>/dev/null
  if grep -q '^# .* KILL' "$HERE/evidence/$label/$tsv" 2>/dev/null; then
    echo "$label: the watcher killed the VM; stopping the series" >&2
    exit 2
  fi
done
