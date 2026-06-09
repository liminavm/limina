#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

set -u
cd ~/Projects/limina
WORK=/tmp/seated-clean.raw; LOG=/tmp/seated-clean-worker.log
rm -f "$LOG"
target/debug/limina --vmm-bin target/debug/limina-vmm \
  --kernel target/test-guest/kernel/Image-16k \
  --cmdline "root=/dev/vda3 rootflags=subvol=root rootfstype=btrfs rw selinux=0 console=ttyAMA0" \
  --disk "$WORK" --cpus 4 --ram-mib 4096 --net --window >"$LOG" 2>&1 &
echo "limina pid=$! (no-clone, disk preserves env edits)"
wait
