#!/usr/bin/env bash
# Does a VM's vCPU scheduling policy move the HOST's wake latency at all?
#
#   hostlate-matrix.sh <disk> [reps]
#
# The first attempt at this used `wakeprobe`, which spends ~80 s on a 6x4 lever matrix to yield one
# sample per cell. Against a heavy tail that is the wrong instrument: reps of one arm disagreed by
# 2 ms to 25 ms, and the ordering flipped between passes. `hostlate` measures the one cell that
# matters — an ordinary thread, `mach_wait_until` — many times, and reports the counts (`over8ms`,
# `over16ms`, half a frame and a whole one) rather than a max, so reps can be pooled.
#
# Each saturated cell proves the guest is loaded before it measures. A cell that fails that check
# is not a loaded arm and says so.
set -uo pipefail
cd "$(dirname "$0")/../.."

disk=$1; reps=${2:-6}
base=$(basename "$disk" .raw)
probe=spikes/macos-timer-wakeup/hostlate
[ -x "$probe" ] || clang -O2 -o "$probe" spikes/macos-timer-wakeup/hostlate.c

cleanup() { pkill -f "^target/debug/limina --vmm-bin.*$base" 2>/dev/null; true; }
trap cleanup EXIT

run_cell() {       # run_cell <label>
  for r in $(seq "$reps"); do
    echo "$1 rep=$r $("$probe" 900 16667)"
  done
}

echo "== no VM (host floor)"
run_cell "floor    guest=-   "

for sched in "" "rt" "rt+dyn"; do
  worker=/tmp/limina-worker-$base.log
  rm -f "$worker"
  LIMINA_DISK="$disk" LIMINA_NET=1 LIMINA_CPUS=8 RUST_LOG=limina=info,krun_vmm=info \
    LIMINA_VCPU_SCHED="$sched" \
    nohup spikes/venus-draw-probe/boot-enhanced-efi-kk.sh > "/tmp/limina-boot-$base.log" 2>&1 &
  boot=$!
  port=$(scripts/wait-guest-ssh.sh "$worker" 300 "$boot") || { echo "arm '$sched': no ssh"; continue; }
  ssh_guest() {
    ssh -p "$port" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
      -o ConnectTimeout=20 claude@127.0.0.1 "$@" 2>/dev/null
  }
  echo "== policy='${sched:-unbanded}'  armed vCPUs: $(grep -c 'VCPU-RT' "$worker" || echo 0)"
  sleep 60

  run_cell "${sched:-unbanded} guest=idle"

  # Detached, and proven: spinners started as ssh children die with the session, and a pkill
  # pattern that matches its own command line kills the shell running it.
  ssh_guest "printf '%s\n' '#!/bin/bash' 'while :; do :; done' > /tmp/limina-spin.sh; \
             chmod +x /tmp/limina-spin.sh; \
             for i in \$(seq \$(nproc)); do setsid nohup timeout 300 /tmp/limina-spin.sh \
             >/dev/null 2>&1 & done; sleep 1"
  sleep 3
  idle=$(ssh_guest "top -bn1 | grep '%Cpu' | sed -E 's/.*, *([0-9.]+) id.*/\1/'")
  if [ -n "$idle" ] && [ "${idle%%.*}" -le 10 ]; then
    echo "   guest saturated (idle=${idle}%)"
    run_cell "${sched:-unbanded} guest=sat "
  else
    echo "!! guest did not saturate (idle=${idle:-?}%) — skipping the loaded cell"
  fi
  ssh_guest "pkill -f 'limina[-]spin' >/dev/null 2>&1; true" >/dev/null

  ssh_guest "echo claudiusrobotus | sudo -S systemctl isolate multi-user.target"
  sleep 3
  ssh_guest "echo claudiusrobotus | sudo -S poweroff"
  for _ in $(seq 24); do pgrep -f "^target/debug/limina-vmm" >/dev/null || break; sleep 5; done
  cleanup
  wait "$boot" 2>/dev/null
done
