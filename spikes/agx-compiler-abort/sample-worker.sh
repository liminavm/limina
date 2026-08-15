#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# Sample the exhaustible host resources of a running limina-vmm while a workload runs, so a
# crash has numbers behind it instead of a theory. Written for the AGX Metal compiler abort
# (task #29): MTLCompilerService aborts on `readBitcode ... [Failed assertion "bitcode_url"]`
# and the worker follows it down, which is the shape of a resource lookup that failed rather
# than a graphics bug -- see the limina-vulkan-oom-lies memory ("ask what refused it") and
# limina-fd-limit-crash (a 256-fd launchd limit produced an unrelated-looking venus abort).
#
# Columns: unix time, open fds, the fd soft limit, vm region count, RSS in MB, threads.
#
# Usage: spikes/agx-compiler-abort/sample-worker.sh [interval_seconds] [outfile]
#   Default: 1s to spikes/agx-compiler-abort/worker-samples.tsv
#
# Read it after a crash with the .ips timestamp in hand: the interesting line is the LAST one
# before the abort, and the interesting question is which column was climbing.
set -uo pipefail

INTERVAL="${1:-1}"
OUT="${2:-$(dirname "$0")/worker-samples.tsv}"

pid=""
while [ -z "$pid" ]; do
  # -x: the SUPERVISOR carries --vmm-bin in its argv and would match a -f search (a trap the
  # suspend-incidents notes cost a session once). The worker is the exact-name match.
  pid=$(pgrep -x limina-vmm | head -1)
  [ -z "$pid" ] && sleep 1
done
echo "sampling limina-vmm pid=$pid every ${INTERVAL}s -> $OUT" >&2

printf 'time\tfds\tfd_limit\tvm_regions\trss_mb\tthreads\n' > "$OUT"
limit=$(launchctl limit maxfiles 2>/dev/null | awk '{print $2}')
[ -z "$limit" ] && limit=0

while kill -0 "$pid" 2>/dev/null; do
  now=$(date +%s)
  fds=$(lsof -p "$pid" 2>/dev/null | wc -l | tr -d ' ')
  # vmmap's own summary line is authoritative and far cheaper to parse than counting regions.
  regions=$(vmmap --summary "$pid" 2>/dev/null | awk '/^ *Regions:/ {print $2; exit}')
  [ -z "$regions" ] && regions=$(vmmap "$pid" 2>/dev/null | grep -c '^[A-Za-z]')
  read -r rss threads <<<"$(ps -o rss=,thcount= -p "$pid" 2>/dev/null | awk '{print int($1/1024), $2}')"
  printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$now" "${fds:-0}" "$limit" "${regions:-0}" "${rss:-0}" "${threads:-0}" >> "$OUT"
  sleep "$INTERVAL"
done

echo "worker $pid exited; $(( $(wc -l < "$OUT") - 1 )) samples in $OUT" >&2
