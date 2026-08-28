#!/usr/bin/env bash
# What does a banded VM charge the HOST?
#
#   host-impact.sh <disk> <LIMINA_VCPU_SCHED> [reps]
#
# The band is a reservation: it takes cores from everything that is not banded, which on a laptop
# is the user's editor, their browser, and any second VM. Every other measurement in this spike
# describes the guest; this one puts the instrument on the host side and asks what an ordinary host
# thread is charged for a 16.667 ms deadline while a VM holds eight reservations.
#
# `wakeprobe` is the same oracle that measured the host's baseline lateness (~1.5 ms median on an
# idle host), so the arms are directly comparable to it. Its `default` policy row is the victim we
# care about; its TIME_CONSTRAINT row says whether a banded host thread escapes.
#
# Repeat the arms. The guest-side collapse is stochastic and there is no reason this is steadier.
set -uo pipefail
cd "$(dirname "$0")/../.."

disk=$1; sched=$2; reps=${3:-2}
base=$(basename "$disk" .raw)

# set -e is off, but a stray failure must still not leave a VM holding the disk — the next arm
# would then fail to boot, which is how two runs of this were lost.
cleanup() {
  pkill -f "^target/debug/limina --vmm-bin.*$base" 2>/dev/null
  true
}
trap cleanup EXIT
worker=/tmp/limina-worker-$base.log
probe=spikes/macos-timer-wakeup/wakeprobe
[ -x "$probe" ] || clang -O2 -o "$probe" spikes/macos-timer-wakeup/wakeprobe.c

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

# Saturating the guest is a differential like any other, and it fails silently in two ways.
# Spinners started as background children of an ssh session take SIGHUP when the session exits, so
# the cell quietly measures an idle guest — hence setsid. And `pkill -f 'while :'` matches the very
# ssh command line carrying it, so the kill takes down its own shell and ssh returns 255; the
# spinners get a marker, and the pattern is bracketed so it cannot match itself.
saturate_guest() {                 # saturate_guest <seconds>
  ssh_guest "printf '%s\\n' '#!/bin/bash' 'while :; do :; done' > /tmp/limina-spin.sh; \
             chmod +x /tmp/limina-spin.sh; \
             for i in \$(seq \$(nproc)); do \
               setsid nohup timeout $1 /tmp/limina-spin.sh >/dev/null 2>&1 & done; sleep 1" || true
  sleep 3
  local idle
  idle=$(ssh_guest "top -bn1 | grep '%Cpu' | sed -E 's/.*, *([0-9.]+) id.*/\\1/'" || true)
  if [ -z "$idle" ] || [ "${idle%%.*}" -gt 10 ]; then
    echo "!! guest did not saturate (idle=${idle:-?}%) — this cell is NOT a loaded arm"
    return 1
  fi
  echo "   guest saturated (idle=${idle}%)"
}

desaturate_guest() {
  ssh_guest "pkill -f 'limina[-]spin' >/dev/null 2>&1; true" >/dev/null || true
}

echo "== policy='${sched:-none}'  armed vCPUs: $(grep -c 'VCPU-RT' "$worker" || echo 0)"
sleep 60   # the desktop's first minute is login jobs, not idle

for state in idle saturated; do
  for r in $(seq "$reps"); do
    if [ "$state" = saturated ]; then
      saturate_guest 120 || true
    fi
    out=$("$probe" 200 16667)
    # The ordinary thread is the victim; the banded one says whether a reservation is an escape.
    def=$(echo "$out" | awk '/^=== policy: default/,/^=== policy: QOS/' | grep 'mach_wait_until')
    tc=$(echo "$out" | awk '/^=== policy: TIME_CONSTRAINT/,0' | grep 'mach_wait_until')
    echo "${sched:-none} guest=$state rep=$r  host-default  $def"
    echo "${sched:-none} guest=$state rep=$r  host-banded   $tc"
    desaturate_guest
    sleep 5
  done
done

ssh_guest "echo claudiusrobotus | sudo -S systemctl isolate multi-user.target" || true
sleep 3
ssh_guest "echo claudiusrobotus | sudo -S poweroff" || true
for _ in $(seq 24); do pgrep -f "^target/debug/limina-vmm" >/dev/null || break; sleep 5; done
pkill -f "^target/debug/limina --vmm-bin.*$base" 2>/dev/null || true
wait "$boot" 2>/dev/null || true
