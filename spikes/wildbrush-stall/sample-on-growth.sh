#!/bin/sh
# Wait for the KK allocator pool to run away, then `sample` the worker — no human timing needed.
# Usage: sample-on-growth.sh <worker-log> <worker-pid> <out-dir>
# Takes a 10 s sample when a class first grows past 200 allocators, and another past 1000.
log=$1; pid=$2; out=$3
for mark in 200 1000; do
  while kill -0 "$pid" 2>/dev/null; do
    if grep -qE "grew to $mark allocators" "$log"; then
      echo "$(date -u +%H:%M:%S) pool crossed $mark — sampling"
      sample "$pid" 10 -file "$out/sample-$mark.txt" >/dev/null 2>&1
      cp "$log" "$out/worker-at-$mark.log"
      break
    fi
    sleep 0.5
  done
done
