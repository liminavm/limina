#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# What the stock guest is doing at one instant of a Basemark stall: who Firefox's threads are
# blocked in, and whether the virtio-gpu fences they could be waiting on are moving. Run twice a
# minute apart; a fence counter that did not advance between the two dumps is a wait nobody will
# end, one that advanced is a guest that is merely slow.
#
# Usage (as the session user, with passwordless sudo): dump-guest.sh <out-dir>
set -u
OUT=$1
mkdir -p "$OUT"
sudo mountpoint -q /sys/kernel/debug || sudo mount -t debugfs none /sys/kernel/debug

date +%s.%N > "$OUT/when"
cat /proc/loadavg > "$OUT/loadavg"

# Every thread of every Firefox-family process (the parent, content, GPU, RDD, socket and utility
# processes all run the same binary), with the kernel function each one sleeps in.
PIDS=$(pgrep -d, -f '[f]irefox' || true)
if [ -n "$PIDS" ]; then
  ps -L -o pid,tid,stat,pcpu,wchan:40,comm,args -p "$PIDS" > "$OUT/threads.txt" 2>&1
  for p in ${PIDS//,/ }; do
    for t in /proc/$p/task/*; do
      echo "=== pid $p tid ${t##*/} [$(cat "$t/comm" 2>/dev/null)] $(cat "$t/wchan" 2>/dev/null)"
      sudo cat "$t/stack" 2>/dev/null
    done
  done > "$OUT/stacks.txt" 2>&1
  # Distinct kernel stacks, most common first, so a wait shared by many threads stands out and a
  # lone GPU wait (dma_fence_wait, virtio_gpu_wait_ioctl, drm_syncobj...) is easy to find.
  awk '/^===/{if (s!="") c[s]++; s=""; next} {sub(/^\[<[0-9a-f]+>\] /,""); s=s $0 ";"} END{if (s!="") c[s]++; for (k in c) print c[k], k}' \
    "$OUT/stacks.txt" | sort -rn > "$OUT/stacks-distinct.txt"
fi

# virtio-gpu's own view of its fences: the last one emitted against the last one the host
# signalled. A gap that does not close between two dumps is the stall's signature.
# `fence <signalled> <emitted>` (virtgpu_debugfs.c); healthy reads equal. debugfs is root-only, so
# the glob has to expand as root too. dri/0, 128 and the mmio name are one device.
sudo sh -c 'D=/sys/kernel/debug/dri/0
  for f in virtio-gpu-irq-fence clients internal_clients gem_names; do echo "=== $f"; cat $D/$f; done
  for c in /sys/kernel/debug/dri/client-*; do echo "=== ${c##*/}"; cat $c/proc_info; done' \
  > "$OUT/virtio-gpu.txt" 2>&1
# Fences attached to shared buffers, and whether each has signalled.
sudo cat /sys/kernel/debug/dma_buf/bufinfo > "$OUT/dma-buf.txt" 2>&1

top -b -H -n 1 -w 200 > "$OUT/top.txt" 2>&1
ss -tnp > "$OUT/sockets.txt" 2>&1
sudo dmesg > "$OUT/dmesg.txt" 2>&1

# The page, if Marionette can still reach it. A content process wedged in a GPU wait cannot run
# the script, so a timeout here is itself an observation.
timeout 20 python3 /tmp/marionette.py js \
  'return [document.location.pathname, document.readyState, (document.body ? document.body.innerText : "").slice(0, 400)]' \
  > "$OUT/page.txt" 2>&1 || echo "marionette: no answer in 20 s (rc=$?)" >> "$OUT/page.txt"

echo "dump written to $OUT"
