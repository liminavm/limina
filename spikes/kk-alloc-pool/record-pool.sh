#!/usr/bin/env bash
# Timestamp the KK allocator-pool reports as they arrive.
#
# The reports are raw fprintf from the Mesa build, so they carry no clock of their own — which is
# useless for correlating a number with what the human was doing at the time. Stamp each line on
# arrival and keep the whole stream: the trajectory is the measurement, not the final value.
#
#   record-pool.sh <worker-log> <out>
set -euo pipefail
log=${1:?worker log}
out=${2:?output file}
: > "$out"
echo "# recording $log -> $out (started $(date -u +%Y-%m-%dT%H:%M:%SZ))" >&2
tail -n +1 -F "$log" 2>/dev/null | grep --line-buffered -E "LIMINA-ALLOC-POOL|USE AFTER DESTROY" |
  while IFS= read -r line; do
    printf '%s %s\n' "$(date -u +%H:%M:%S)" "$line" >> "$out"
  done
