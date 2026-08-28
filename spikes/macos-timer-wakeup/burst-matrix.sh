#!/usr/bin/env bash
# Sweep burst length against one vCPU scheduling policy.
#
#   burst-matrix.sh <disk> <LIMINA_VCPU_SCHED> [burst-seconds...]
#
# One boot per policy (the worker reads it once, at vCPU start). Installs mangohud if the image
# lacks it — the F44 enhanced image does, and without it the layer never loads and every run
# reports NO LOG.
set -euo pipefail
cd "$(dirname "$0")/../.."

disk=$1; sched=$2; shift 2
bursts="${*:-0.25 0.5 1 2}"
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
echo "== policy='$sched'  armed vCPUs at boot: $(grep -c 'VCPU-RT' "$worker" || echo 0)"

# sshd answers before the guest's network is usable, and a dnf that fails there used to abort the
# whole run under set -e — leaving the VM up, so the next arm collided with its own disk.
for _ in 1 2 3 4 5; do
  ssh_guest "rpm -q mangohud" >/dev/null 2>&1 && break
  ssh_guest "echo claudiusrobotus | sudo -S dnf install -y mangohud" >/dev/null 2>&1 || true
  sleep 15
done
ssh_guest "rpm -q mangohud" >/dev/null 2>&1 || echo "!! mangohud missing — this arm will report NO LOG"
scp -q -P "$port" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
  spikes/macos-timer-wakeup/burstrun.sh claude@127.0.0.1:/tmp/ 2>/dev/null
ssh_guest "chmod +x /tmp/burstrun.sh"
for b in $bursts; do
  ssh_guest "/tmp/burstrun.sh ${sched:-none} $b 3"
done

ssh_guest "echo claudiusrobotus | sudo -S systemctl isolate multi-user.target" || true
sleep 3
ssh_guest "echo claudiusrobotus | sudo -S poweroff" || true
for _ in $(seq 24); do pgrep -f "^target/debug/limina-vmm" >/dev/null || break; sleep 5; done
pkill -f "^target/debug/limina --vmm-bin.*$base" 2>/dev/null || true
wait "$boot" 2>/dev/null || true
