#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# Connect (or disconnect) a spare pool slot at runtime, by writing a `display` line straight to
# the worker's display-control socket — the same wire the supervisor's own migration path uses
# (crates/limina-displayctl). Proves the mechanism the whole pool design rests on: a slot that
# booted disconnected can be given an identity and become a monitor without rebuilding the device.
#
#   spikes/scanout-pool/hotplug-slot.sh <limina-pid> <slot> on  [WxH]
#   spikes/scanout-pool/hotplug-slot.sh <limina-pid> <slot> off
set -u
PID="${1:?usage: hotplug-slot.sh <limina-pid> <slot> on|off [WxH]}"
SLOT="${2:?slot}"
ACTION="${3:?on|off}"
SIZE="${4:-1920x1080}"
SOCK="${TMPDIR:-/tmp}limina-resize-$PID.sock"
[ -S "$SOCK" ] || { echo "no display-control socket at $SOCK"; exit 1; }

case "$ACTION" in
  on)
    # A full identity, so the guest sees a distinguishable monitor rather than the anonymous
    # krun-display fallback. serial/product are arbitrary but must be non-zero and unique.
    LINE="display id=$SLOT size=$SIZE connected=1 refresh=60 dpi=109 vendor=LMN"
    LINE="$LINE product=$((0x5000 + SLOT)) serial=$((0x50000000 + SLOT)) name=pool%20slot%20$SLOT"
    ;;
  off) LINE="display id=$SLOT connected=0" ;;
  *)   echo "action must be on|off"; exit 1 ;;
esac

echo "-> $LINE"
printf '%s\n' "$LINE" | nc -U "$SOCK"
