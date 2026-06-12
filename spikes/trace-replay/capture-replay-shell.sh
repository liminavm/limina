#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Phase 4 of the trace-replay plan: capture GNOME SHELL ITSELF (mutter compositing on
# zink->venus — THE desktop workload) as a replayable apitrace fixture, and pull it to
# fixtures/traces/gnome-shell-seated.trace (the venus_shell_replay fixture).
#
# Usage: spikes/trace-replay/capture-replay-shell.sh
# Needs: a seated CLONE up with --net booted from /tmp/seated-kk.raw
# (plain spikes/venus-draw-probe/boot-seated-kk.sh) — the script reboots the guest
# twice and reuses that disk, so do NOT point it at the golden.
#
# How the capture works (proven 2026-06-12, see RESULTS.md):
# - gnome-shell is a systemd user unit (org.gnome.Shell@wayland.service); a drop-in
#   sets LD_PRELOAD=egltrace.so. It must sort LAST (zz-*.conf) — the existing gdb.conf
#   drop-in re-sets LD_PRELOAD and the later file wins.
# - NO TRACE_FILE: each traced process (shell, Xwayland) auto-names its own trace in
#   $HOME, so children can't corrupt a shared file.
# - The trace finalizes on clean process exit -> disarm + poweroff, pull on next boot.
# - Capture protocol: SEATED IDLE OVERVIEW ONLY, no client windows. Client window
#   content (dmabuf imports of other processes' buffers) cannot replay and produces
#   backend-divergent undefined pixels; the first frames (startup fade) sample
#   uninitialized textures and also diverge. The test drops the first snapshot pair
#   and compares the rest — on this protocol they reproduce pixel-exact.
set -euo pipefail
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
cd "$ROOT"

SSH=(ssh -p 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR claude@127.0.0.1)
DISK=/tmp/seated-kk.raw
[ -f "$DISK" ] || { echo "no $DISK — boot a clone first (boot-seated-kk.sh)"; exit 1; }

wait_ssh() {
  for _ in $(seq 1 36); do
    sleep 5
    "${SSH[@]}" -o ConnectTimeout=2 "$1" 2>/dev/null && return 0
  done
  echo "guest never came up ($1)"; exit 1
}

echo "== arming the capture drop-in + rebooting into a traced session"
"${SSH[@]}" '
rm -f ~/*.trace
mkdir -p ~/.config/systemd/user/org.gnome.Shell@wayland.service.d
printf "[Service]\nEnvironment=LD_PRELOAD=/usr/lib64/apitrace/wrappers/egltrace.so\n" \
  > ~/.config/systemd/user/org.gnome.Shell@wayland.service.d/zz-apitrace.conf
sudo reboot' || true
sleep 12

LIMINA_DISK="$DISK" bash spikes/venus-draw-probe/boot-seated-kk.sh >/tmp/shell-capture-boot.log 2>&1 &
wait_ssh 'pgrep -x gnome-shell >/dev/null && ls ~/gnome-shell.trace'

echo "== seated under the tracer; idling for frames, then disarming + finalizing"
"${SSH[@]}" '
sleep 8
ls -la ~/gnome-shell.trace
rm ~/.config/systemd/user/org.gnome.Shell@wayland.service.d/zz-apitrace.conf
sync
sudo poweroff' || true
sleep 12

LIMINA_DISK="$DISK" bash spikes/venus-draw-probe/boot-seated-kk.sh >/tmp/shell-capture-boot2.log 2>&1 &
wait_ssh 'ls /run/user/1000/.mutter-Xwaylandauth.*'

mkdir -p fixtures/traces
scp -P 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR \
    claude@127.0.0.1:gnome-shell.trace fixtures/traces/gnome-shell-seated.trace
echo "fixture updated: fixtures/traces/gnome-shell-seated.trace (guest left running)"
