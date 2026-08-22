#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# Boot an image with a virtio-gpu scanout POOL of N (slot 0 connected, the rest disconnected)
# through the normal EFI+venus path, so the pool is the only variable.
#
#   spikes/scanout-pool/boot-with-pool.sh 4 [disk.raw]
#
# Leaves the VM running in the background; probe it with probe-connectors.sh and read the SSH
# port out of the worker log the same way any other boot does.
set -u
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
cd "$ROOT"
POOL="${1:?usage: boot-with-pool.sh <pool 1..16> [disk.raw]}"
DISK="${2:-Fedora-Workstation-44.pool-spike.raw}"
[ -f "$DISK" ] || { echo "no such disk: $DISK (clone one with cp -c)"; exit 1; }

# A worker without com.apple.security.hypervisor dies at `hv_vm_create` with
# `Internal(Vm(VmSetup(VmCreate)))`, which reads as a VM/image problem and is nothing of the
# sort. The signature survives only until the next cargo invocation that relinks the binary —
# any plain `cargo build` touching a different package set is enough — so check it here rather
# than diagnose it again. Cost this spike half an hour on 2026-08-17.
if ! codesign -d --entitlements - target/debug/limina-vmm 2>&1 | grep -q "com.apple.security.hypervisor"; then
  echo "target/debug/limina-vmm has lost its hypervisor entitlement — run: cargo xtask build" >&2
  exit 1
fi

export LIMINA_DISPLAY_POOL="$POOL"
export LIMINA_DISK="$DISK"
# Keyed on BOTH the pool and the disk: keying on the pool alone made a stock run at pool=4
# truncate the enhanced pool=4 run's log, which is the same evidence-destroying collision the
# per-disk default in boot-enhanced-efi-kk.sh exists to prevent.
export LIMINA_BOOT_LOG="/tmp/limina-pool$POOL-$(basename "${DISK%.raw}").log"
exec spikes/venus-draw-probe/boot-enhanced-efi-kk.sh
