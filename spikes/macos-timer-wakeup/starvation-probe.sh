#!/usr/bin/env bash
# Watch what the host threads are doing while a banded, saturated guest collapses.
#
#   starvation-probe.sh <disk> [LIMINA_VCPU_SCHED value]
#
# Boots with the venus ring wake profile on, saturates the guest, and samples the worker's
# per-thread CPU while vkcube runs. If the collapse is host-side starvation, the vCPU threads hold
# the cores and the present path (gpu worker, ring thread) is left with nothing.
set -euo pipefail
cd "$(dirname "$0")/../.."

disk=$1; sched=${2:-rt}
base=$(basename "$disk" .raw)
worker=/tmp/limina-worker-$base.log
rm -f "$worker"

LIMINA_DISK="$disk" LIMINA_NET=1 LIMINA_CPUS=8 RUST_LOG=limina=info,krun_vmm=info \
  LIMINA_VCPU_SCHED="$sched" LIMINA_RING_WAKE_PROFILE=1 \
  nohup spikes/venus-draw-probe/boot-enhanced-efi-kk.sh > "/tmp/limina-boot-$base.log" 2>&1 &
boot=$!
port=$(scripts/wait-guest-ssh.sh "$worker" 300 "$boot")
ssh_guest() {
  ssh -p "$port" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
    claude@127.0.0.1 "$@" 2>/dev/null
}
scp -q -P "$port" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
  spikes/macos-timer-wakeup/fpsrun.sh claude@127.0.0.1:/home/claude/fpsrun.sh 2>/dev/null
ssh_guest "chmod +x fpsrun.sh"

pid=$(pgrep -f "^target/debug/limina-vmm" | head -1)
[ -z "$pid" ] && pid=$(pgrep -x limina-vmm | head -1)
echo "worker pid $pid"
ssh_guest "./fpsrun.sh probe saturated" &
guest=$!
sleep 12
for i in 1 2 3; do
  echo "--- per-thread CPU sample $i"
  ps -M "$pid" | head -16
  sleep 4
done
wait "$guest" || true

echo "--- ring wake profile"
grep -h "RINGWAKE" "$worker" | tail -6 || echo "(no ring wake lines)"
echo "--- band lines"
grep -h "VCPU-RT" "$worker" | tail -4 || true

ssh_guest "echo claudiusrobotus | sudo -S systemctl isolate multi-user.target" || true
sleep 3
ssh_guest "echo claudiusrobotus | sudo -S poweroff" || true
wait "$boot" 2>/dev/null || true
