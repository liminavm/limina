#!/usr/bin/env bash
# What the scheduling policy costs an idle guest, in worker CPU and idle wakeups.
#
#   idle-cost.sh <disk> [LIMINA_VCPU_SCHED value]
#
# The idle-wakeup budget is one the project spent effort winning (docs/design/venus-ring-idle-
# wakeups.md), so any punctuality mechanism reports what it hands back. Samples only after the
# desktop has settled — the first samples after boot are all login, not idle.
set -euo pipefail
cd "$(dirname "$0")/../.."

disk=$1; sched=${2:-}
base=$(basename "$disk" .raw)
worker=/tmp/limina-worker-$base.log
rm -f "$worker"

LIMINA_DISK="$disk" LIMINA_NET=1 LIMINA_CPUS=8 RUST_LOG=limina=info,krun_vmm=info \
  LIMINA_VCPU_SCHED="$sched" \
  nohup spikes/venus-draw-probe/boot-enhanced-efi-kk.sh > "/tmp/limina-boot-$base.log" 2>&1 &
boot=$!
port=$(scripts/wait-guest-ssh.sh "$worker" 300 "$boot")
ssh_guest() {
  ssh -p "$port" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
    claude@127.0.0.1 "$@" 2>/dev/null
}

pid=$(pgrep -f "^target/debug/limina-vmm" | head -1)
echo "policy='$sched' worker pid $pid — settling"
sleep 60
echo "idle samples (cpu%, idle wakeups/s):"
top -l 8 -s 10 -stats pid,cpu,idlew -pid "$pid" | grep -E "^ *$pid " || true

ssh_guest "echo claudiusrobotus | sudo -S systemctl isolate multi-user.target" || true
sleep 3
ssh_guest "echo claudiusrobotus | sudo -S poweroff" || true
wait "$boot" 2>/dev/null || true
