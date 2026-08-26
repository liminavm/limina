#!/usr/bin/env bash
# Boot the poke VM for the LIMINA_TEXT_NOCACHE arm, with the debug channels already open.
#
# LIMINA_GLOBAL_SCANOUT=1 is NOT optional: without it the scanout IOSurfaces are Mach-port-scoped,
# iosdump binds a stale surface from early boot, and every card scores NO BANNER -- which reads as
# 100% damage instead of as a broken oracle. That fault voided a whole day's arms once.
#
#   nocache-boot.sh          # boots, waits for ssh, prints the port and the worker log
set -eu
cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
DISK="${LIMINA_DISK:-poke-stock-0824.raw}"
SCRATCH="${LIMINA_SCRATCH:-$PWD/poke-build-scratch.raw}"
LOG="/tmp/limina-worker-${DISK%.raw}.log"

LIMINA_GLOBAL_SCANOUT=1 RUST_LOG=limina=info LIMINA_POINTER_WIRE_TRACE=1 \
nohup xtask/target/debug/xtask run --disk "$DISK" -- --disk "$SCRATCH" >/tmp/nocache-boot.log 2>&1 &

port=$(scripts/wait-guest-ssh.sh "$LOG" 300)
spikes/notification-text-corruption/ensure-input.sh "$port" >/dev/null
echo
echo "ssh port   : $port"
echo "worker log : $LOG"
echo "  scanout ids:  grep -m1 'scanout 0 -> IOSurfaces' $LOG"
echo "  live trace :  tail -f $LOG"
