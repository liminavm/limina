#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Guest-side idle-wakeup probe: run INSIDE the enhanced Fedora guest over ssh.
# Confirms kernel/tickless config and measures per-CPU timer interrupt rate at idle,
# which maps 1:1 onto host vCPU-thread wakeups in the libkrun/HVF VMM.
set -u
echo "===== KERNEL / PAGE ====="
uname -r
echo "PAGE_SIZE=$(getconf PAGE_SIZE)  nproc=$(nproc)"
echo "cmdline: $(cat /proc/cmdline)"
echo "uptime:$(uptime)"
echo
echo "===== TICKLESS CONFIG ====="
if [ -r /proc/config.gz ]; then
  zcat /proc/config.gz | grep -E '^CONFIG_(NO_HZ|HZ=|HZ_PERIODIC|HZ_[0-9]|HIGH_RES_TIMERS|RCU_NOCB)' || echo "(none matched)"
else
  echo "/proc/config.gz not present; checking /boot config"
  grep -E '^CONFIG_(NO_HZ|HZ=|HZ_PERIODIC|HZ_[0-9]|HIGH_RES_TIMERS|RCU_NOCB)' /boot/config-"$(uname -r)" 2>/dev/null || echo "(no /boot config)"
fi
echo "timer_migration=$(cat /proc/sys/kernel/timer_migration 2>/dev/null)"
echo
echo "===== TOP CPU CONSUMERS (idle desktop) ====="
top -bn1 -o +%CPU 2>/dev/null | head -18
echo
echo "===== PER-CPU INTERRUPT DELTA over 5s (idle) ====="
cp /proc/interrupts /tmp/irq1
cp /proc/stat /tmp/stat1
sleep 5
cp /proc/interrupts /tmp/irq2
cp /proc/stat /tmp/stat2
echo "--- /proc/interrupts lines whose total changed (per-CPU delta over 5s) ---"
paste <(cat /tmp/irq1) <(cat /tmp/irq2) >/dev/null 2>&1 || true
# Compute deltas with awk keyed by irq label
awk '
  NR==FNR {
    if (FNR==1) next
    lbl=$1; s=0; for(i=2;i<=NF;i++){ if($i ~ /^[0-9]+$/){a[lbl","(i-1)]=$i} }
    next
  }
  FNR==1 { next }
  {
    lbl=$1; line=lbl; changed=0; tot=0
    for(i=2;i<=NF;i++){
      if($i ~ /^[0-9]+$/){
        d=$i-a[lbl","(i-1)];
        line=line"\t"d; tot+=d; if(d!=0)changed=1
      } else { line=line"\t"$i }
    }
    if(changed) printf "%-8s total_delta=%d  per5s=%s\n", lbl, tot, line
  }
' /tmp/irq1 /tmp/irq2 | sort -t= -k2 -nr | head -30
echo
echo "--- context switches / total interrupts over 5s (from /proc/stat) ---"
c1=$(awk '/^ctxt/{print $2}' /tmp/stat1); c2=$(awk '/^ctxt/{print $2}' /tmp/stat2)
i1=$(awk '/^intr/{print $2}' /tmp/stat1); i2=$(awk '/^intr/{print $2}' /tmp/stat2)
echo "ctxt_switches/5s=$((c2-c1))  (~$(( (c2-c1)/5 ))/s)"
echo "total_intr/5s=$((i2-i1))     (~$(( (i2-i1)/5 ))/s)"
echo
echo "===== /proc/timer_list summary (active hrtimers count) ====="
grep -c 'hrtimer_' /proc/timer_list 2>/dev/null || echo "n/a"
echo "done"
