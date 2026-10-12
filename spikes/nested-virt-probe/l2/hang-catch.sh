#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Runs as root INSIDE a nested limina guest, after l2/setup.sh. Boots a one-vCPU L2 over and over
# until one hangs, then records what the hung one is doing: whether its vCPU thread burns CPU or
# sleeps, its registers from the QEMU monitor, and whether L1's KVM still takes exits for it.
#
#   hang-catch.sh [runs]      (default 30)
set -uo pipefail
L2=/root/l2
runs=${1:-30}
mon=/tmp/l2-mon.sock
out=/tmp/l2-hang
mkdir -p "$out"

monitor() {
    python3 - "$mon" "$1" <<'PY'
import socket, sys, time
s = socket.socket(socket.AF_UNIX)
s.connect(sys.argv[1])
s.settimeout(2)
def drain():
    buf = b""
    try:
        while True:
            d = s.recv(65536)
            if not d:
                break
            buf += d
    except socket.timeout:
        pass
    return buf.decode(errors="replace")
drain()
s.sendall((sys.argv[2] + "\n").encode())
time.sleep(0.5)
print(drain())
PY
}

kvm_dir() { ls -d /sys/kernel/debug/kvm/*-* 2>/dev/null | head -n 1; }

kvm_exits() {
    local d
    d=$(kvm_dir)
    [ -n "$d" ] && grep -H . "$d"/exits "$d"/wfe_exit_stat "$d"/wfi_exit_stat \
        "$d"/halt_wakeup "$d"/halt_successful_poll "$d"/halt_attempted_poll \
        "$d"/halt_poll_invalid "$d"/irq_exits "$d"/mmio_exit_kernel "$d"/mmio_exit_user \
        2>/dev/null
}

mountpoint -q /sys/kernel/debug || mount -t debugfs none /sys/kernel/debug
for i in $(seq 1 "$runs"); do
    rm -f "$mon"
    qemu-system-aarch64 -M virt,gic-version=3,highmem=off -accel kvm -cpu host -smp 1 -m 512 \
        -kernel "/boot/vmlinuz-$(uname -r)" -initrd "$L2/initramfs.cpio.gz" \
        -append "console=ttyAMA0 rdinit=/init" \
        -drive "file=$L2/disk.img,if=none,format=raw,id=d0" -device virtio-blk-pci,drive=d0 \
        -display none -monitor "unix:$mon,server,nowait" -serial stdio \
        -no-reboot </dev/null >"$out/console.log" 2>&1 &
    pid=$!
    done=no
    for _ in $(seq 1 30); do
        sleep 0.5
        if ! kill -0 "$pid" 2>/dev/null; then
            done=yes
            break
        fi
    done
    if [ "$done" = yes ]; then
        echo "run $i: finished"
        continue
    fi
    echo "run $i: HUNG after 15 s; recording"
    {
        echo "== console tail"
        tail -n 5 "$out/console.log"
        echo "== threads (tid, %cpu, cumulative time, name), then again 3 s later"
        ps -L -o tid,pcpu,time,comm -p "$pid"
        kvm_exits >"$out/exits-1.txt"
        sleep 3
        ps -L -o tid,pcpu,time,comm -p "$pid"
        kvm_exits >"$out/exits-2.txt"
        echo "== L1 KVM counters, 3 s apart"
        paste "$out/exits-1.txt" "$out/exits-2.txt"
        echo "== L1 KVM vgic state, twice 1 s apart"
        cat "$(kvm_dir)/vgic-state"
        sleep 1
        cat "$(kvm_dir)/vgic-state"
        echo "== L1 KVM trace, 50 ms"
        t=/sys/kernel/tracing
        echo >"$t/trace"
        echo 1 >"$t/events/kvm/enable"
        sleep 0.05
        echo 0 >"$t/events/kvm/enable"
        sed -E 's/^ *[^ ]+ +\[[0-9]+\] [^ ]+ +[0-9.]+: //' "$t/trace" | sed -E 's/0x[0-9a-f]+/X/g; s/[0-9]+/N/g' \
            | sort | uniq -c | sort -rn
        echo "== L1 KVM trace, first 60 lines"
        grep -v '^#' "$t/trace" | head -n 60
        echo "== monitor: info registers"
        monitor "info registers"
        echo "== monitor: info irq"
        monitor "info irq"
        echo "== L1 dmesg tail"
        dmesg | tail -n 10
    } >"$out/hang-$i.txt" 2>&1
    kill "$pid"
    wait "$pid" 2>/dev/null
    echo "recorded in $out/hang-$i.txt"
    exit 0
done
echo "no hang in $runs runs"
