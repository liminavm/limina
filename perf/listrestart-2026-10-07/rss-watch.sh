#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Runs ON the Mac hosting a VM: once a second, logs the worker's memory and the host's, and kills
# the VM before it can take the host down. Local to that Mac on purpose: when the host is swamped,
# ssh from elsewhere is the first thing to stop answering.
#
# Usage: rss-watch.sh <disk-name> <out.tsv> <footprint-limit-MiB> <compressor-growth-MiB> <max-seconds>
#   <disk-name>  matched against the worker's command line (the VM's disk), so only this VM is touched
#
# RSS alone does not protect the host: under pressure the worker's pages move into the compressor,
# so its RSS FALLS while the host runs out (measured: RSS 3-6 GB while top's footprint, which counts
# compressed pages, was 8-11 GB). The kill therefore fires on the worker's footprint (top's MEM) or
# on the host compressor's growth since the watch began, or when <max-seconds> pass. Growth, not
# size: a host can sit at 5.4 GB compressed at rest (pages left behind by killed workers), and a
# fixed cap then leaves almost no headroom.
set -u
DISK="$1"; OUT="$2"; FP_LIMIT="$3"; COMP_LIMIT="$4"; MAX_S="$5"
PAGE=$(sysctl -n hw.pagesize)
host_comp_mib() {
  local c
  c=$(vm_stat | awk '/occupied by compressor/ {gsub(/\./,"",$5); print $5}')
  echo $((c * PAGE / 1048576))
}
COMP_BASE=$(host_comp_mib)
# The VM's own processes: the supervisor and worker binaries inside the app bundle, anchored to the
# start of the command line so a shell that merely mentions them does not match. A looser
# "limina.*<disk>" also matched the shell that launched this watcher, whose command line names the
# bundle's directory and the disk, and killed it.
VM_PAT="^[^ ]*Contents/MacOS/limina.*$DISK"
start=$(date +%s)
echo "# compressor at start: ${COMP_BASE} MiB; kill at +${COMP_LIMIT} MiB" > "$OUT"
echo -e "time\tworker_pid\trss_mib\tfootprint_mib\tworker_compressed_mib\tfree_mib\tcompressor_mib" >> "$OUT"

# Every process of this VM seen so far. pgrep -f reads each candidate's arguments from its memory,
# and under the pressure this guard exists for it can come back empty for a process that is
# plainly alive: it once matched nothing at the moment of a kill, one second after it had found
# the worker, and the VM ran on. So the kill also takes every pid already seen, and checks.
known=""
kill_vm() {
  echo "# $(date +%T) KILL: $1" >> "$OUT"
  local p left
  for p in $known $(pgrep -f "$VM_PAT"); do
    kill -0 "$p" 2>/dev/null || continue
    ps -ww -o pid=,command= -p "$p" | cut -c1-160 >> "$OUT"
    kill -9 "$p" 2>/dev/null
  done
  sleep 2
  left=""
  for p in $known; do kill -0 "$p" 2>/dev/null && left="$left $p"; done
  if [ -n "$left" ]; then
    echo "# $(date +%T) STILL ALIVE after kill -9:$left" >> "$OUT"
  else
    echo "# $(date +%T) killed" >> "$OUT"
  fi
}

# top prints sizes as 9055M / 10G / 512K; convert to MiB.
to_mib() {
  case "$1" in
    *G) echo "$1" | awk '{printf "%d", substr($1, 1, length($1) - 1) * 1024}' ;;
    *M) echo "$1" | awk '{printf "%d", substr($1, 1, length($1) - 1)}' ;;
    *K) echo 0 ;;
    *) echo 0 ;;
  esac
}

# The watch belongs to the first supervisor that appears and ends with it. Points of a series reuse
# one disk name, so a watch that outlived its own VM adopted the next point's and killed it on its
# own time limit.
sup=""; waited=0
while :; do
  now=$(date +%s)
  if [ $((now - start)) -ge "$MAX_S" ]; then kill_vm "time limit ${MAX_S}s"; break; fi
  if [ -z "$sup" ]; then
    sup=$(pgrep -f "^[^ ]*Contents/MacOS/limina --disk .*$DISK" | head -n 1)
    if [ -z "$sup" ]; then
      waited=$((waited + 1))
      [ "$waited" -ge 120 ] && { echo "# $(date +%T) no VM appeared in 120 s" >> "$OUT"; break; }
      sleep 1; continue
    fi
    known="$sup"
    echo "# $(date +%T) watching supervisor $sup" >> "$OUT"
  fi
  kill -0 "$sup" 2>/dev/null || { echo "# $(date +%T) supervisor $sup gone" >> "$OUT"; break; }
  # A guest reboot replaces the worker under the same supervisor; wait for the new one.
  w=$(pgrep -f "^[^ ]*Contents/MacOS/limina-vmm.*$DISK" | head -n 1)
  [ -z "$w" ] && { sleep 1; continue; }
  for p in $w $(pgrep -f "$VM_PAT"); do
    case " $known " in *" $p "*) ;; *) known="$known $p" ;; esac
  done
  rss_kb=$(ps -o rss= -p "$w" | tr -d ' ')
  read -r fp cmp < <(top -l 1 -pid "$w" -stats mem,cmprs | tail -n 1)
  fp_mib=$(to_mib "${fp:-0}"); cmp_mib=$(to_mib "${cmp:-0}")
  vm=$(vm_stat)
  free=$(echo "$vm" | awk '/Pages free/ {gsub(/\./,"",$3); print $3}')
  comp=$(echo "$vm" | awk '/occupied by compressor/ {gsub(/\./,"",$5); print $5}')
  comp_mib=$((comp * PAGE / 1048576))
  echo -e "$(date +%T)\t$w\t$((${rss_kb:-0} / 1024))\t$fp_mib\t$cmp_mib\t$((free * PAGE / 1048576))\t$comp_mib" >> "$OUT"
  if [ "$fp_mib" -ge "$FP_LIMIT" ]; then kill_vm "worker footprint ${fp_mib} MiB >= ${FP_LIMIT} MiB"; break; fi
  if [ "$comp_mib" -ge $((COMP_BASE + COMP_LIMIT)) ]; then
    kill_vm "host compressor ${comp_mib} MiB >= ${COMP_BASE} + ${COMP_LIMIT} MiB"; break
  fi
  sleep 1
done
