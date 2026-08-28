#!/usr/bin/env bash
# Frame-time matrix for one vCPU scheduling policy, idle and loaded, in a real guest.
#
#   arm-matrix.sh <disk> <label> [LIMINA_VCPU_SCHED value] [modes...]
#
# Boots the disk in place, waits for sshd, copies `fpsrun.sh` into the guest and runs it for the
# idle and loaded arms twice each, then powers the guest off. The guest needs mangohud and vkcube. The policy is a worker
# env var, so it is fixed for the life of a boot — one boot per arm is the point of this script.
set -euo pipefail
cd "$(dirname "$0")/../.."

disk=$1; label=$2; sched=${3:-}; shift 3 || true
modes="${*:-idle loaded saturated}"
base=$(basename "$disk" .raw)
worker=/tmp/limina-worker-$base.log
rm -f "$worker"

LIMINA_DISK="$disk" LIMINA_NET=1 LIMINA_CPUS=8 RUST_LOG=limina=info,krun_vmm=info LIMINA_VCPU_SCHED="$sched" \
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

grep -h "VCPU-RT" "$worker" | head -1 || echo "(no band line: policy off)"
for rep in 1 2; do
  for mode in $modes; do
    ssh_guest "./fpsrun.sh '$label' $mode" | sed "s/\$/ rep$rep/"
  done
done

grep -h "VCPU-RT" "$worker" | tail -3 || true

# A seated GNOME session refuses poweroff through its inhibitors; drop the session first.
ssh_guest "echo claudiusrobotus | sudo -S systemctl isolate multi-user.target" || true
sleep 3
ssh_guest "echo claudiusrobotus | sudo -S poweroff" || true
wait "$boot" 2>/dev/null || true
