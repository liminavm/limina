#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# B3 diagnostic — why didn't the GPIO KEY_SLEEP button suspend the stock guest?
# Tests premises P1..P4 with the gpio debug log on and input/udev/logind introspection.
set -uo pipefail
REPO=~/Projects/limina
JOB="${CLAUDE_JOB_DIR:-~/.claude/jobs/916f2b3c}"
LOG="$JOB/tmp/b3-diag.log"; DISK="$JOB/tmp/b3-diag.raw"; BOOTLOG="$JOB/tmp/b3-diag-boot.log"
PORT=2224
SSH="ssh -p $PORT -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5 -o LogLevel=ERROR claude@127.0.0.1"
: > "$LOG"; say(){ echo "[$(date '+%H:%M:%S')] $*" | tee -a "$LOG"; }
cd "$REPO"
say "cloning..."; cp -c Fedora-Workstation-44.accessible.raw "$DISK" || { say "clone failed"; exit 1; }
say "booting with RUST_LOG=krun_devices=debug on port $PORT..."
RUST_LOG=krun_devices=debug caffeinate -dimsu "$REPO/target/debug/limina" \
  --firmware "$REPO/target/krun-efi/KRUN_EFI.gop.fd" \
  --disk "$DISK" --cpus 2 --ram-mib 4096 --net --ssh-port "$PORT" > "$BOOTLOG" 2>&1 &
LP=$!; say "limina pid=$LP"
up=0; for i in $(seq 1 60); do $SSH true 2>/dev/null && { up=1; say "SSH up ~$((i*4))s"; break; }; sleep 4; done
[ "$up" = 1 ] || { say "no boot"; kill -9 $LP 2>/dev/null; exit 1; }
WPID=$(pgrep -f "limina-vmm.*b3-diag.raw" | head -1); say "worker pid=$WPID"

say "=== P3a: gpio-keys input device block (/proc/bus/input/devices) ==="
$SSH 'cat /proc/bus/input/devices' 2>/dev/null | awk '/gpio-keys/{p=1} p{print} /^$/{if(p)exit}' | tee -a "$LOG"

say "=== P3b: evtest --query for KEY_RESTART(0x198=408) and KEY_SLEEP(142) on each input event dev ==="
$SSH 'for e in /dev/input/event*; do n=$(cat /sys/class/input/$(basename $e)/device/name 2>/dev/null);
  sudo evtest --query $e EV_KEY KEY_RESTART; r=$?; sudo evtest --query $e EV_KEY KEY_SLEEP; s=$?;
  echo "$e [$n] KEY_RESTART_exit=$r KEY_SLEEP_exit=$s"; done' 2>/dev/null | tee -a "$LOG"

say "=== P4a: udev power-switch tag on the gpio-keys event dev ==="
$SSH 'for e in /dev/input/event*; do n=$(cat /sys/class/input/$(basename $e)/device/name 2>/dev/null);
  [ "$n" = "gpio-keys" ] && { echo "== $e =="; udevadm info "$e" | grep -iE "TAGS|CURRENT_TAGS|ID_INPUT"; }; done' 2>/dev/null | tee -a "$LOG"

say "=== P4b: logind key handling config ==="
$SSH 'systemctl show systemd-logind -p HandleSuspendKey -p HandlePowerKey -p HandleHibernateKey 2>/dev/null; echo "---logind.conf---"; grep -vE "^#|^$" /etc/systemd/logind.conf 2>/dev/null || true' 2>/dev/null | tee -a "$LOG"

# find the gpio-keys event dev number for the capture
EVDEV=$($SSH 'for e in /dev/input/event*; do n=$(cat /sys/class/input/$(basename $e)/device/name 2>/dev/null); [ "$n" = "gpio-keys" ] && { echo $e; break; }; done' 2>/dev/null | tr -d "\r")
say "gpio-keys evdev = $EVDEV"

say "=== P1..P3: capture input events on $EVDEV for 8s while pressing the suspend button ==="
$SSH "sudo dmesg -C; sudo bash -c 'timeout 8 evtest $EVDEV > /tmp/evt.log 2>&1 &'" 2>/dev/null
sleep 1
say "pressing suspend button (SIGUSR2 -> $WPID)..."
kill -USR2 "$WPID"
sleep 3
say "--- worker gpio debug log (grep key/gpio/irq) ---"
grep -iE 'suspend key|restart key|gpio|SET_IRQ' "$BOOTLOG" | tail -20 | tee -a "$LOG"
say "--- guest evtest capture (/tmp/evt.log) ---"
$SSH 'cat /tmp/evt.log 2>/dev/null | tail -25' 2>/dev/null | tee -a "$LOG"
say "--- guest dmesg since press ---"
$SSH 'sudo dmesg | tail -15' 2>/dev/null | tee -a "$LOG"
say "--- logind journal since boot (did it see a key?) ---"
$SSH 'journalctl -u systemd-logind --no-pager | tail -15' 2>/dev/null | tee -a "$LOG"

say "cleanup"; kill -9 $LP 2>/dev/null; pkill -9 -f "b3-diag.raw" 2>/dev/null; rm -f "$DISK"; say "diag done."
