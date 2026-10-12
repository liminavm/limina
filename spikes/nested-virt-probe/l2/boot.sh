#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Runs as root INSIDE a nested limina guest, after l2/setup.sh. Boots the guest's own kernel as an
# L2 guest under QEMU+KVM with a virtio-blk disk and a PL011 serial console, and checks what the
# L2 init reports.
#
#   boot.sh          boot to L2-DONE and verify the disk read; prints the wall time
#   boot.sh tick     boot an L2 that ticks once a second on its console forever, logging to
#                    /root/l2/tick.log (run it as a systemd unit: it never exits)
#
# `highmem=off`: the guest's KVM offers a 36-bit IPA space, and the virt machine's high MMIO
# regions sit above it.
set -euo pipefail
L2=/root/l2
mode=${1:-once}
append="console=ttyAMA0 rdinit=/init ${L2_QUIET-quiet}"
[ "$mode" = tick ] && append="$append l2.tick"
qemu=(qemu-system-aarch64 -M virt,gic-version=3,highmem=off -accel kvm -cpu host -smp 2 -m 512
    -kernel "/boot/vmlinuz-$(uname -r)" -initrd "$L2/initramfs.cpio.gz" -append "$append"
    -drive "file=$L2/disk.img,if=none,format=raw,id=d0" -device virtio-blk-pci,drive=d0
    -display none -monitor none -serial stdio -no-reboot)

# The console goes to stdout and the tick log is a redirect: QEMU's `-serial file:` backend,
# under systemd-run, spun at 80% CPU and stopped writing after the first line.
if [ "$mode" = tick ]; then
    exec "${qemu[@]}" </dev/null >"$L2/tick.log" 2>&1
fi

start=$(date +%s.%N)
rc=0
out=$(timeout "${L2_TIMEOUT:-300}" "${qemu[@]}" 2>&1) || rc=$?
end=$(date +%s.%N)
echo "$out" >"$L2/last-console.log"
grep -o 'L2-[A-Z]*.*' "$L2/last-console.log" || true
want=$(cat "$L2/disk.md5")
got=$(grep -o 'L2-DISK [0-9a-f]\{32\}' "$L2/last-console.log" | cut -d' ' -f2 || true)
if [ "$rc" = 0 ] && grep -q L2-DONE "$L2/last-console.log" && [ "$got" = "$want" ]; then
    printf 'L2 PASS: booted, read its virtio disk (md5 %s), powered off in %.1f s\n' "$got" \
        "$(echo "$end - $start" | bc)"
else
    echo "L2 FAIL: qemu exit $rc (124 = timed out), disk md5 '$got', want '$want';" \
        "console in $L2/last-console.log"
    exit 1
fi
