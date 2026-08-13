#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Boot a Fedora raw clone under qemu-system-aarch64 -accel hvf, headless,
# ssh forwarded to 127.0.0.1:2299. Prints the qemu pid.
set -e
cd "$(dirname "$0")"

DISK="${1:?usage: run-qemu.sh <clone.raw>}"
CODE=/opt/homebrew/share/qemu/edk2-aarch64-code.fd
VARS=qemu-efi-vars.fd
[ -f "$VARS" ] || dd if=/dev/zero of="$VARS" bs=1m count=64 2>/dev/null

qemu-system-aarch64 \
    -M virt -accel hvf -cpu host -smp 4 -m 8192 \
    -drive if=pflash,format=raw,readonly=on,file="$CODE" \
    -drive if=pflash,format=raw,file="$VARS" \
    -drive if=virtio,format=raw,file="$DISK" \
    -netdev user,id=n0,hostfwd=tcp:127.0.0.1:2299-:22 \
    -device virtio-net-pci,netdev=n0 \
    -display none -serial file:qemu-serial.log \
    -daemonize -pidfile qemu.pid

echo "qemu pid: $(cat qemu.pid)  ssh: ssh -p 2299 claude@127.0.0.1"
