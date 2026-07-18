#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# M9.2 in-place WAKE spike (Fable rec #1): can a wakeup-source GPIO button wake an s2idle guest that
# armed NO wakealarm? And does a NON-wake button leave it frozen? Settles the restore-half wake design
# before any snapshot code. No snapshot/restore here — pure in-place s2idle on a live guest.
#   suspend (SIGUSR2/KEY_SLEEP) -> s2idle, no rtcwake
#   baseline: stays frozen on its own
#   negative: pulse suspend button again (KEY_SLEEP, NOT wakeup-source) -> expect STILL frozen
#   positive: pulse wake button (SIGWINCH/KEY_WAKEUP, wakeup-source)   -> expect WAKE (SSH back)
set -uo pipefail
REPO=~/Projects/limina
JOB="${CLAUDE_JOB_DIR:-~/.claude/jobs/916f2b3c}"
LOG="$JOB/tmp/wake-spike.log"; DISK="$JOB/tmp/wake-spike.raw"; BOOTLOG="$JOB/tmp/wake-spike-boot.log"
PORT=2229
SSH="ssh -p $PORT -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5 -o LogLevel=ERROR claude@127.0.0.1"
: > "$LOG"; say(){ echo "[$(date '+%H:%M:%S')] $*" | tee -a "$LOG"; }
wpid(){ pgrep -f "limina-vmm.*wake-spike.raw" | head -1; }
ssh_up(){ $SSH true 2>/dev/null; }
frozen_for(){ # returns 0 if SSH stays DOWN for $1 polls of 2s
  local n=$1; for _ in $(seq 1 "$n"); do sleep 2; ssh_up && return 1; done; return 0; }
cd "$REPO"
say "cloning..."; cp -c Fedora-Workstation-44.accessible.raw "$DISK" || { say "clone failed"; exit 1; }
say "booting stock F44 EFI (RUST_LOG gpio debug) on $PORT..."
RUST_LOG=krun_devices=debug caffeinate -dimsu "$REPO/target/debug/limina" \
  --firmware "$REPO/target/krun-efi/KRUN_EFI.gop.fd" \
  --disk "$DISK" --cpus 2 --ram-mib 2048 --net --ssh-port "$PORT" > "$BOOTLOG" 2>&1 &
LP=$!; say "limina pid=$LP"
up=0; for i in $(seq 1 60); do ssh_up && { up=1; say "SSH up ~$((i*4))s"; break; }; sleep 4; done
[ "$up" = 1 ] || { say "no boot"; kill -9 $LP 2>/dev/null; exit 1; }
W=$(wpid); say "worker=$W"
# confirm the wake button enumerated with wakeup-source
say "guest gpio-keys wakeup arming:"
$SSH 'for e in /sys/class/input/event*; do n=$(cat $e/device/name 2>/dev/null); [ "$n" = gpio-keys ] && cat $e/device/../power/wakeup 2>/dev/null; done; grep -c wakeup /proc/interrupts 2>/dev/null; cat /sys/power/mem_sleep' 2>/dev/null | tee -a "$LOG"

$SSH 'sudo dmesg -C' 2>/dev/null
say "SUSPEND: SIGUSR2 -> $W (s2idle, no wake armed)"; kill -USR2 "$W"
if frozen_for 6; then say "guest froze (SSH down)"; else say "FAIL: guest did not freeze"; kill -9 $LP; exit 1; fi

say "BASELINE: wait ~12s untouched -> expect STILL frozen (no spurious wake)"
if frozen_for 6; then say "baseline OK: stayed frozen"; else say "NOTE: guest woke on its own (spurious wake)"; fi

say "NEGATIVE: pulse SUSPEND button again (SIGUSR2/KEY_SLEEP, not wakeup-source) -> expect still frozen"
kill -USR2 "$W"
if frozen_for 5; then NEG="stayed frozen (GOOD: non-wake irq did not wake)"; else NEG="WOKE (unexpected: non-wake irq woke s2idle)"; fi
say "NEGATIVE result: $NEG"

say "POSITIVE: pulse WAKE button (SIGWINCH/KEY_WAKEUP, wakeup-source) -> expect WAKE"
kill -WINCH "$W"
woke=0; for i in $(seq 1 15); do sleep 2; ssh_up && { woke=1; say "SSH BACK (poll $i) — guest woke"; break; }; done

say "=== gpio debug (suspend/wake key events) ==="; grep -iE 'suspend key|wake key|power key' "$BOOTLOG" | tail -8 | tee -a "$LOG"
say "=== guest PM dmesg ==="; $SSH 'sudo dmesg | grep -iE "suspend entry|suspend exit|resume" | tail -6' 2>/dev/null | tee -a "$LOG"
say "----"
if [ "$woke" = 1 ]; then say "VERDICT: WAKE-SPIKE GREEN — wakeup-source button wakes s2idle; negative: $NEG"; else say "VERDICT: WAKE-SPIKE FAIL — wake button did NOT wake the guest"; fi
say "cleanup"; kill -9 $LP 2>/dev/null; pkill -9 -f "wake-spike.raw" 2>/dev/null; rm -f "$DISK"; say "spike done."
