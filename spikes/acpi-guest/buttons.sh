#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# The power, sleep and wake buttons on an ACPI guest (docs/design/firmware-and-acpi.md, phase 1).
# Under ACPI the PL061 lines are GPIO-signalled events that notify PNP0C0C/PNP0C0E buttons; the
# wake line is wake-capable. This is spikes/m9-freeze-trigger/wake-spike.sh for that machine:
#   suspend (SIGUSR2 -> sleep line) -> MODE (s2idle or deep), no wakealarm
#   baseline: stays frozen on its own
#   negative: the sleep line again (not wake-capable) -> expect still frozen
#   positive: the wake line (SIGWINCH, wake-capable) -> expect the guest back
#   stop: SIGTERM -> the power line -> expect a clean power-off without the guest agent
#
# Usage: [MODE=s2idle|deep] spikes/acpi-guest/buttons.sh <disk.raw> [extra limina args...]
# MODE picks /sys/power/mem_sleep (default s2idle). Only s2idle tests the ACPI wake line: in deep
# (PSCI SYSTEM_SUSPEND) the VMM itself resumes the guest on the wake eventfd.
# Boots the disk in place, headless (clone it first: cp -c). Run from a worktree with a built
# tree and target/firmware/efi.fd.
set -uo pipefail
cd "$(dirname "$0")/../.."
DISK="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"; shift
LOG="${LOG:-/tmp/limina-acpi-buttons.log}"
MODE="${MODE:-s2idle}"
: > "$LOG"
say() { echo "[$(date '+%H:%M:%S')] $*"; }

RUST_LOG=warn,limina=info,krun_devices=debug target/debug/limina \
    --firmware target/firmware/efi.fd --disk "$DISK" --cpus 2 --ram-mib 2048 --net \
    --machine acpi --discard-suspend "$@" > "$LOG" 2>&1 < /dev/null &
LP=$!
say "limina pid=$LP, log $LOG"
port=$(gtimeout --foreground 300 scripts/wait-guest-ssh.sh "$LOG" 240 "$LP") || { say "no boot"; kill -9 "$LP"; exit 1; }
SSH="ssh -p $port -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5 -o LogLevel=ERROR claude@127.0.0.1"
ssh_up() { $SSH true 2>/dev/null; }
frozen_for() { for _ in $(seq 1 "$1"); do sleep 2; ssh_up && return 1; done; return 0; }
W=$(pgrep -P "$LP" -f '[l]imina-vmm' || pgrep -f "[l]imina-vmm.*$(basename "$DISK")" | head -1)
say "ssh port $port, worker $W"

say "guest view:"
$SSH 'ls /sys/firmware/acpi/tables | tr "\n" " "; echo;
      ls /sys/bus/acpi/devices | grep -E "PNP0C0C|PNP0C0E|ARMH0061" | tr "\n" " "; echo;
      grep -H . /sys/class/input/input*/name | grep -i button;
      grep -E "pl061|GPIO|ACPI:Event" /proc/interrupts;
      cat /sys/power/mem_sleep;
      sudo dmesg | grep -iE "acpi.*(error|warn)|gpio.*acpi|button" ' 2>&1

$SSH "sudo dmesg -C; echo $MODE | sudo tee /sys/power/mem_sleep" 2>/dev/null
say "SUSPEND ($MODE): SIGUSR2 -> $W (sleep line)"
kill -USR2 "$W"
if frozen_for 8; then say "guest froze (SSH down)"; else say "FAIL: guest did not freeze"; SUSPEND=fail; fi

if [ "${SUSPEND:-}" != fail ]; then
    say "BASELINE: ~12 s untouched -> expect still frozen"
    if frozen_for 6; then say "baseline OK"; else say "NOTE: woke on its own"; fi
    say "NEGATIVE: sleep line again (not wake-capable) -> expect still frozen"
    kill -USR2 "$W"
    if frozen_for 5; then NEG="stayed frozen"; else NEG="WOKE"; fi
    say "negative: $NEG"
    say "POSITIVE: SIGWINCH -> $W (wake line)"
    kill -WINCH "$W"
    woke=0
    for i in $(seq 1 20); do sleep 2; ssh_up && { woke=1; say "SSH back (poll $i)"; break; }; done
    $SSH 'sudo dmesg | grep -iE "PM: suspend|suspend entry|suspend exit|wake|button"' 2>&1
    if [ "$woke" = 1 ]; then say "VERDICT wake: GREEN (negative: $NEG)"; else say "VERDICT wake: FAIL"; fi
fi

say "STOP: SIGTERM -> $LP (the power line; the guest agent is not asked for ~5 s)"
t0=$(date +%s)
kill -TERM "$LP"
for _ in $(seq 1 60); do kill -0 "$LP" 2>/dev/null || break; sleep 1; done
if kill -0 "$LP" 2>/dev/null; then
    say "VERDICT stop: FAIL (still running after 60 s); killing"
    kill -TERM "$LP"; sleep 2; kill -9 "$LP" 2>/dev/null
else
    say "VERDICT stop: powered off in $(( $(date +%s) - t0 )) s"
fi
grep -E "power button|qga: asked|powered off|did not take" "$LOG"
