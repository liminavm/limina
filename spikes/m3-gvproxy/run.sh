#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# M3 gvproxy NAT spike — does our virtio-net wiring (libkrun UnixgramPath + VFKT magic)
# actually handshake with the installed gvproxy, and does a stock Fedora guest then get a
# DHCP lease and reach the outside world?
#
# Oracle is HOST-SIDE: stock Fedora is silent on serial after GRUB (no console= on the
# pristine image), so we can't read NetworkManager from the guest console. Instead we read
# gvproxy's -debug log: a DHCP lease grant + outbound (DNS/TCP) traffic prove the whole
# path (handshake -> link -> DHCP -> NAT) end to end.
#
# Drives limina-vmm DIRECTLY (the supervisor doesn't spawn gvproxy yet — that's the next,
# productization step). Headless: no display, so it also dodges the local-Terminal GPU hang.
set -uo pipefail
cd "$(dirname "$0")/../.."

SECS="${SECS:-90}"
IMAGE="Fedora-Workstation-43.raw"
SCRATCH="Fedora-Workstation-43.m3-spike.raw"   # *.raw gitignored
FW="${FW:-/opt/homebrew/share/krunkit/KRUN_EFI.silent.fd}"
OUT="spikes/m3-gvproxy/out"
# Must be ABSOLUTE: gvproxy parses `unixgram://host/path`, so a relative path's first
# component is mistaken for the URL host. An absolute path yields `unixgram:///abs/path`.
SOCK="$(pwd)/$OUT/gvproxy-net.sock"
mkdir -p "$OUT"
rm -f "$OUT"/*.log "$SOCK"

cleanup() { kill -9 "${GP:-}" "${VMM:-}" 2>/dev/null; pkill -9 -f target/debug/limina-vmm 2>/dev/null; rm -f "$SCRATCH" "$SOCK" "${SOCK}-krun.sock"; }
trap cleanup EXIT

echo "==> clone $IMAGE -> $SCRATCH (APFS cow)"
rm -f "$SCRATCH"; cp -c "$IMAGE" "$SCRATCH"

echo "==> launch gvproxy (-listen-vfkit unixgram://$SOCK)"
gvproxy -debug -listen-vfkit unixgram://"$SOCK" >"$OUT/gvproxy.log" 2>&1 &
GP=$!

# Wait for gvproxy to create the listening socket before booting.
for _ in $(seq 1 50); do [ -S "$SOCK" ] && break; sleep 0.1; done
[ -S "$SOCK" ] || { echo "!! gvproxy never created $SOCK"; exit 1; }
echo "    socket up: $SOCK"

echo "==> boot Fedora headless via limina-vmm with --net-gvproxy for ${SECS}s"
RUST_LOG="info,krun_devices::virtio::net=debug" \
  target/debug/limina-vmm \
    --firmware "$FW" --disk "$SCRATCH" \
    --cpus 4 --ram-mib 4096 \
    --net-gvproxy "$SOCK" \
    --console "$OUT/console.log" \
    >"$OUT/worker.log" 2>&1 &
VMM=$!

sleep "$SECS"

echo
echo "==> RESULTS (gvproxy.log)"
echo "--- DHCP / lease ---"
grep -iE "dhcp|lease|offer|192\.168\.127" "$OUT/gvproxy.log" | head -20 || echo "(none)"
echo "--- outbound (DNS/TCP) ---"
grep -iE "tcp|udp|dns|forward|dial|connect" "$OUT/gvproxy.log" | head -20 || echo "(none)"
echo
echo "==> worker net log (handshake)"
grep -iE "net|vfkit|unixgram|eth0" "$OUT/worker.log" | head -20 || echo "(none)"
echo
echo "==> artifacts in $OUT/ (gvproxy.log, worker.log, console.log)"
