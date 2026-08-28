#!/usr/bin/env bash
# Does the banded collapse need a BUSY HOST?
#
#   host-contention.sh <disk> <LIMINA_VCPU_SCHED> [host-spinner-counts...]
#
# The collapse was first measured with another VM running on the same host, and did not reproduce
# on an idle one. The band is a reservation, so the threads it starves are the worker's *own*
# non-vCPU threads — the venus ring thread above all — and how badly depends on what else wants a
# core. So: same guest saturation, sweeping how many host threads compete for what is left.
#
# Host spinners run at ordinary priority; they cannot preempt a banded vCPU, which is the point.
# They compete with everything that is *not* banded, which is where the present path lives.
set -euo pipefail
cd "$(dirname "$0")/../.."

disk=$1; sched=$2; shift 2
counts="${*:-0 4 8}"
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
    -o ConnectTimeout=20 claude@127.0.0.1 "$@" 2>/dev/null
}
echo "== policy='${sched:-none}'  armed vCPUs: $(grep -c 'VCPU-RT' "$worker" || echo 0)"
for _ in 1 2 3 4 5; do
  ssh_guest "rpm -q mangohud" >/dev/null 2>&1 && break
  ssh_guest "echo claudiusrobotus | sudo -S dnf install -y mangohud" >/dev/null 2>&1 || true
  sleep 15
done
scp -q -P "$port" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
  spikes/macos-timer-wakeup/fpsrun.sh claude@127.0.0.1:/tmp/ 2>/dev/null
ssh_guest "chmod +x /tmp/fpsrun.sh"

for n in $counts; do
  pids=()
  for _ in $(seq "$n" 2>/dev/null || true); do
    yes > /dev/null & pids+=($!)
  done
  [ "$n" -gt 0 ] && sleep 2
  for mode in saturated idle; do
    echo "host=$n $(ssh_guest "/tmp/fpsrun.sh host$n $mode" 2>&1 | grep -v Aborted)"
  done
  for p in ${pids[@]+"${pids[@]}"}; do kill "$p" 2>/dev/null || true; done
  sleep 3
done

ssh_guest "echo claudiusrobotus | sudo -S systemctl isolate multi-user.target" || true
sleep 3
ssh_guest "echo claudiusrobotus | sudo -S poweroff" || true
for _ in $(seq 24); do pgrep -f "^target/debug/limina-vmm" >/dev/null || break; sleep 5; done
pkill -f "^target/debug/limina --vmm-bin.*$base" 2>/dev/null || true
wait "$boot" 2>/dev/null || true
