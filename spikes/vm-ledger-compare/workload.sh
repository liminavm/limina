#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Drive the page-cache workload in the guest and sample the VMM process
# around it. Usage: workload.sh "<ssh args>" <vmm-pid> <label>
# e.g. workload.sh "-p 2299 claude@127.0.0.1" 12345 qemu
set -e
SSH_ARGS="${1:?ssh args}"
PID="${2:?vmm pid}"
LABEL="${3:?label}"
SSH="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null $SSH_ARGS"

sample() {
    rss_kb=$(ps -o rss= -p "$PID" | tr -d ' ')
    fp=$(sudo -n footprint -p "$PID" 2>/dev/null | awk '/phys_footprint/{print $2; exit}')
    cached_kb=$($SSH "awk '/^Cached:/{print \$2}' /proc/meminfo" 2>/dev/null || echo "?")
    echo "$LABEL,$1,rss_mib=$((rss_kb / 1024)),footprint=${fp:-n/a},guest_cached_mib=$((cached_kb / 1024))"
}

echo "=== $LABEL: pid $PID ==="
sample "pre-drop"
$SSH "sync; echo 3 | sudo tee /proc/sys/vm/drop_caches >/dev/null"
sleep 3
sample "post-drop"
$SSH "sudo dd if=/dev/vda of=/dev/null bs=1M count=6000 2>&1 | tail -1"
sleep 3
sample "post-dd-6G"
