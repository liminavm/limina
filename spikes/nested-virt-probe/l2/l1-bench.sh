#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Runs as root INSIDE a limina guest. What EL2 costs a guest that never runs a VM: the same
# workloads, booted once with --nested-virt and once without, compared. Prints one line per
# workload, `name seconds` (lower is better); `perf bench sched pipe` reports usecs/op.
set -euo pipefail
rpm -q perf >/dev/null || dnf -y -q install perf >/dev/null
t() { local s e; s=$(date +%s.%N); "$@" >/dev/null 2>&1; e=$(date +%s.%N); echo "$e - $s" | bc; }

echo "forkexec_3000 $(t bash -c 'for i in $(seq 3000); do /bin/true; done')"
echo "syscalls_dd_4M $(t dd if=/dev/zero of=/dev/null bs=512 count=4000000)"
echo "pagefault_2G $(t python3 -c 'b = bytearray(2 << 30); b[::16384] = b"\1" * len(b[::16384])')"
echo "cpu_sha256_1G $(t bash -c 'head -c 1G /dev/zero | sha256sum')"
echo "timer_sleep_1ms_x2000 $(t python3 -c 'import time; [time.sleep(0.001) for _ in range(2000)]')"
echo "disk_read_1G $(t dd if=/dev/vda of=/dev/null bs=1M count=1024 iflag=direct)"
pipe=$(perf bench sched pipe -l 200000 2>&1 | awk '/usecs\/op/ {print $1}')
echo "sched_pipe_usecs_per_op $pipe"
idle_wake=$(perf bench sched pipe -l 20000 -T 2>&1 | awk '/usecs\/op/ {print $1}')
echo "sched_pipe_threads_usecs_per_op $idle_wake"
