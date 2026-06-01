#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# limina M1 boot spike — build, codesign, and boot Fedora-Workstation-43.raw via EFI.
#
# Run from this directory. Needs the Homebrew libkrun bottle (link spike — NOT the
# from-source build) and the krunkit EFI firmware blob. Must run un-sandboxed
# (network/codesign) — in Claude Code pass dangerouslyDisableSandbox.
set -euo pipefail
cd "$(dirname "$0")"

FW="${FW:-/opt/homebrew/Cellar/krunkit/1.2.1/share/krunkit/KRUN_EFI.silent.fd}"
DISK="${DISK:-../../Fedora-Workstation-43.raw}"
RAM_MIB="${RAM_MIB:-4096}"
READONLY="${READONLY:-0}"          # 1 = protect the image (GRUB visible, kernel may stall)

# 1. Build + ad-hoc codesign with the hypervisor entitlement (required for hv_vm_create).
clang -O2 -Wall -o boot boot.c -L/opt/homebrew/lib -lkrun
codesign --entitlements hv.entitlements -s - --force boot

# 2. Serial input FIFO (opened O_RDWR by boot.c: kqueue-pollable, never EOFs).
[ -p in.fifo ] || mkfifo in.fifo
rm -f console.log

# 3. Boot. krun_start_enter never returns; kill when done. Tail console.log to watch.
#
#    NOTE: a STOCK Fedora image shows only firmware+GRUB on serial (no console= in its
#    BLS cmdline; the kernel logs to the framebuffer). To see the full kernel dmesg over
#    serial, apply serial-grub.cfg to the image's FAT ESP first (see RESULTS.md "Making
#    the kernel talk on serial"): it adds earlycon=pl011,mmio32,0x0a001000 + console=ttyAMA0.
echo "booting (ro=$READONLY) — tail -f console.log to watch; pkill -x boot to stop"
exec ./boot "$FW" "$DISK" "$PWD/console.log" "$RAM_MIB" "$READONLY" "$PWD/in.fifo"
