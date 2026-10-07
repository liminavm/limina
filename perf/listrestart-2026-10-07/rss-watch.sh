#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Runs ON the Mac hosting a VM: once a second, logs the worker's memory and the host's, and kills
# the VM before it can take the host down. Local to that Mac on purpose: when the host is swamped,
# ssh from elsewhere is the first thing to stop answering.
#
# Usage: rss-watch.sh <disk-name> <out.tsv> <footprint-limit-MiB> <compressor-limit-MiB> <max-seconds>
#   <disk-name>  matched against the worker's command line (the VM's disk), so only this VM is touched
#
# RSS alone does not protect the host: under pressure the worker's pages move into the compressor,
# so its RSS FALLS while the host runs out (measured: RSS 3-6 GB while top's footprint, which counts
# compressed pages, was 8-11 GB). The kill therefore fires on the worker's footprint (top's MEM) or
# on the host compressor's size, or when <max-seconds> pass. A guest reboot replaces the worker
# process, so it follows the new pid and exits only after 90 s with no worker at all.
set -u
DISK="$1"; OUT="$2"; FP_LIMIT="$3"; COMP_LIMIT="$4"; MAX_S="$5"
PAGE=$(sysctl -n hw.pagesize)
start=$(date +%s)
echo -e "time\tworker_pid\trss_mib\tfootprint_mib\tworker_compressed_mib\tfree_mib\tcompressor_mib" > "$OUT"

kill_vm() {
  echo "# $(date +%T) KILL: $1" >> "$OUT"
  for p in $(pgrep -f "[l]imina.*$DISK"); do
    ps -ww -o pid=,command= -p "$p" | cut -c1-160 >> "$OUT"
    kill -9 "$p"
  done
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

seen=0; absent=0
while :; do
  now=$(date +%s)
  if [ $((now - start)) -ge "$MAX_S" ]; then kill_vm "time limit ${MAX_S}s"; break; fi
  w=$(pgrep -f "[l]imina-vmm.*$DISK" | head -n 1)
  if [ -z "$w" ]; then
    if [ "$seen" = 1 ]; then
      absent=$((absent + 1))
      [ "$absent" -ge 90 ] && { echo "# $(date +%T) worker gone for 90 s" >> "$OUT"; break; }
    fi
    sleep 1; continue
  fi
  seen=1; absent=0
  rss_kb=$(ps -o rss= -p "$w" | tr -d ' ')
  read -r fp cmp < <(top -l 1 -pid "$w" -stats mem,cmprs | tail -n 1)
  fp_mib=$(to_mib "${fp:-0}"); cmp_mib=$(to_mib "${cmp:-0}")
  vm=$(vm_stat)
  free=$(echo "$vm" | awk '/Pages free/ {gsub(/\./,"",$3); print $3}')
  comp=$(echo "$vm" | awk '/occupied by compressor/ {gsub(/\./,"",$5); print $5}')
  comp_mib=$((comp * PAGE / 1048576))
  echo -e "$(date +%T)\t$w\t$((${rss_kb:-0} / 1024))\t$fp_mib\t$cmp_mib\t$((free * PAGE / 1048576))\t$comp_mib" >> "$OUT"
  if [ "$fp_mib" -ge "$FP_LIMIT" ]; then kill_vm "worker footprint ${fp_mib} MiB >= ${FP_LIMIT} MiB"; break; fi
  if [ "$comp_mib" -ge "$COMP_LIMIT" ]; then kill_vm "host compressor ${comp_mib} MiB >= ${COMP_LIMIT} MiB"; break; fi
  sleep 1
done
