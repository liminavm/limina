#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# coglrun.sh <COGL_DEBUG value> <tag>
# One cycle of the cogl-knob differential on the SEATED desktop: inject COGL_DEBUG into the guest's
# environment.d (over SSH on the CURRENTLY-RUNNING boot), sync, reboot the worker on the same disk so a
# fresh autologin imports it, then env-gated dock+full capture. Requires a guest already booted with --net
# on /tmp/seated-clean.raw (boot-noclone.sh). Leaves the new boot running for the next cycle.
set -u
cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
COGL="$1"; TAG="$2"
SSH=(ssh -o ConnectTimeout=6 -p 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null claude@127.0.0.1)
echo ">>> inject COGL_DEBUG=$COGL"
for i in $(seq 1 40); do "${SSH[@]}" "echo up" >/dev/null 2>&1 && break; sleep 3; done
"${SSH[@]}" "mkdir -p ~/.config/environment.d; printf 'COGL_DEBUG=%s\n' '$COGL' > ~/.config/environment.d/zz-cogldebug.conf; cat ~/.config/environment.d/zz-cogldebug.conf; sync; sync; sleep 1"
echo ">>> reboot worker (fresh login imports env)"
pkill -f 'limina --vmm-bin' 2>/dev/null; pkill -f 'limina-vmm' 2>/dev/null; pkill -f limina-gvproxy 2>/dev/null
sleep 3
bash spikes/venus-draw-probe/boot-noclone.sh > /tmp/boot-$TAG.out 2>&1 &
echo ">>> capture (gated on COGL_DEBUG active)"
sleep 6
bash spikes/venus-draw-probe/gatedcap.sh "$TAG" COGL_DEBUG
