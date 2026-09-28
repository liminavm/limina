#!/bin/sh
# Host-side 1 Hz sampler of one limina-vmm worker: CPU, RSS, threads, fds, plus host-wide
# pageout / compressor deltas (does the worker's growth push the host into paging?).
# Usage: host-mon.sh <worker-pid> > host-mon.log
pid=$1
n=0
while kill -0 "$pid" 2>/dev/null; do
  vm=$(vm_stat | awk -F: '/Pageouts|Swapouts|Pages occupied by compressor|Pages stored in compressor|Compressions|Decompressions/ {gsub(/[ .]/,"",$2); printf "%s=%s ", $1, $2}' | tr -d '"' | sed 's/Pages occupied by compressor/cmp_occ/; s/Pages stored in compressor/cmp_stored/')
  echo "=== $(date -u +%H:%M:%S) $(ps -o pcpu=,rss= -p "$pid") thr=$(ps -M -p "$pid" | awk 'NR>1' | wc -l | tr -d ' ') | $vm"
  # vmmap is slow; every 10 s only
  if [ $((n % 10)) -eq 0 ]; then
    vmmap -summary "$pid" 2>/dev/null | grep -E 'Physical footprint:' | tr -s ' '
  fi
  n=$((n + 1))
  sleep 1
done
