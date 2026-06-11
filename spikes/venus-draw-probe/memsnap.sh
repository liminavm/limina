#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# memsnap.sh <tag> — snapshot host-worker + guest memory state, append to $MEMLOG
# (default /tmp/memlog.txt). Host: RSS/VSZ, phys footprint, vmmap regions of interest
# (guest RAM = VM_ALLOCATE, venus blobs/IOSurfaces, Metal/IOAccelerator, malloc).
# Guest: free -m + top-RSS processes. Used for the tier-2 memory-overhead rounds.
set -u
TAG=${1:?usage: memsnap.sh <tag>}
LOG=${MEMLOG:-/tmp/memlog.txt}
VMM=$(pgrep limina-vmm | head -1)
[ -n "$VMM" ] || { echo "no limina-vmm running" >&2; exit 1; }
{
   echo "=== $TAG  $(date +%H:%M:%S)  pid=$VMM ==="
   ps -o rss=,vsz= -p "$VMM" |
      awk '{printf "host worker: RSS %.2f GiB  VSZ %.2f GiB\n", $1/1048576, $2/1048576}'
   footprint "$VMM" 2>/dev/null | grep -m1 -i "phys footprint" || true
   vmmap --summary "$VMM" 2>/dev/null |
      awk '/REGION TYPE/{p=1} p && (/TOTAL/ || /IOSurface/ || /IOAccelerator/ || /MALLOC/ || /VM_ALLOCATE/ || /mapped file/ || /Performance tool/ || /REGION TYPE/)' |
      head -12
   echo "--- guest ---"
   ssh -p 2222 -o ConnectTimeout=5 claude@127.0.0.1 \
      'free -m | head -2; echo; ps -eo rss,comm --sort=-rss | head -7' 2>/dev/null ||
      echo "guest ssh unavailable"
   echo
} >>"$LOG"
echo "snapshot $TAG -> $LOG"
