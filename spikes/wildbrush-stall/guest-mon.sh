#!/bin/sh
# Guest-side 1 Hz sampler: load, PSI, memory, and a real (interval, not lifetime) CPU view from
# `top -b`, keeping every task above 2% CPU or in D state.
while :; do
  echo "=== $(date -u +%H:%M:%S.%3N) load $(cut -d' ' -f1-4 /proc/loadavg)"
  for r in cpu memory io; do echo "psi.$r $(head -1 /proc/pressure/$r)"; done
  grep -E '^(MemTotal|MemFree|MemAvailable|Cached|SwapFree|Shmem|Committed_AS):' /proc/meminfo | tr -s ' ' | tr '\n' ' '; echo
  top -b -n 2 -d 0.5 -H -w 200 | awk '/^top -/{n++} n==2 && (/^ *PID/ || ($9+0)>2.0 || $8=="D")'
  sleep 0.5
done
