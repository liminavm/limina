#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Boot a disk headless, run one command in the guest over ssh, then stop the VM: the check for
# each device docs/design/firmware-and-acpi.md describes under ACPI.
#
# Usage: spikes/acpi-guest/probe.sh <disk.raw> '<guest command>' [limina args...]
#   e.g. spikes/acpi-guest/probe.sh acpi-stock.raw 'ls /dev/tpm*' --tpm
# The machine is ACPI unless you pass --machine device-tree, for comparison. Boots
# the disk in place (clone it first: cp -c). Run from a worktree with a built tree and
# target/firmware/efi.fd.
set -uo pipefail
cd "$(dirname "$0")/../.."
DISK="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
CMD="$2"
shift 2
LOG="${LOG:-/tmp/limina-acpi-probe.log}"
: > "$LOG"

RUST_LOG=warn,limina=info,krun_devices=info target/debug/limina \
    --firmware target/firmware/efi.fd --disk "$DISK" --cpus 2 --ram-mib 2048 --net \
    --discard-suspend "$@" > "$LOG" 2>&1 < /dev/null &
LP=$!
echo "limina pid=$LP, log $LOG"
if port=$(gtimeout --foreground 300 scripts/wait-guest-ssh.sh "$LOG" 240 "$LP"); then
    ssh -p "$port" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
        -o ConnectTimeout=5 -o LogLevel=ERROR claude@127.0.0.1 "$CMD"
    rc=$?
else
    echo "no boot"
    rc=1
fi
kill -TERM "$LP"
for _ in $(seq 1 60); do kill -0 "$LP" 2>/dev/null || break; sleep 1; done
if kill -0 "$LP" 2>/dev/null; then
    echo "still running 60 s after the stop; killing"
    kill -TERM "$LP"; sleep 2; kill -9 "$LP" 2>/dev/null
fi
exit "$rc"
