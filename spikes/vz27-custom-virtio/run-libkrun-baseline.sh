#!/bin/sh
# The libkrun baseline: the same initramfs under limina-vmm, so the echo numbers have an
# owned-VMM reference on the same host. The guest finds no command channel (log-only mode);
# hvc0 is libkrun's virtio-console, echoed verbatim through two FIFOs; vsock port 5001 goes to a
# UNIX-socket echo server. Kernel console is the PL011.
#
# usage: run-libkrun-baseline.sh <limina-vmm> <kernel Image> <initramfs.cpio> <workdir>
set -e
vmm=$1 kernel=$2 initrd=$3 work=$4
here=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$work"
rm -f "$work/hvc-out" "$work/hvc-in" "$work/echo.sock"
mkfifo "$work/hvc-out" "$work/hvc-in"
python3 -I "$here/echo-helpers.py" fifo "$work/hvc-out" "$work/hvc-in" &
fifo_pid=$!
python3 -I "$here/echo-helpers.py" unix "$work/echo.sock" &
unix_pid=$!
sleep 0.5
"$vmm" --kernel "$kernel" --initramfs "$initrd" --cmdline "console=ttyAMA0 rdinit=/init vzprobe.only=echo vzprobe.nobulk" \
    --cpus 2 --ram-mib 2048 --console "$work/console.log" \
    --virtio-console "$work/hvc-out" --virtio-console-input "$work/hvc-in" \
    --vsock-port 5001 --vsock-socket "$work/echo.sock" --no-snd --no-battery || true
kill $fifo_pid $unix_pid 2>/dev/null || true
grep -a RESULT "$work/console.log" || true
