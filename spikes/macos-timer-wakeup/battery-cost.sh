#!/usr/bin/env bash
# What the vCPU scheduling band costs the battery.
#
#   battery-cost.sh <disk> <block-minutes> <arm> [arm...]
#
# An arm is `<LIMINA_VCPU_SCHED>:<load>`, load being `idle` or `vkcube` — an empty policy is the
# unbanded baseline, and the arm `none:host` runs no VM at all, which is the floor every other
# block is read against, and the load says whether anything is presenting. Each arm needs its own boot
# (the policy is read once, at vCPU start), so interleave them: the pack's voltage sags as it
# drains, and running A then B would hand the second arm a different battery.
#
# Oracle: the pack's own draw, InstantAmperage (mA, two's complement) x Voltage (mV), sampled every
# 5 s, plus the AppleRawCurrentCapacity drop across the block. powermetrics reports the package
# rather than the whole machine, but it needs root; this needs none, and it is the quantity the
# user actually spends. Every sample is kept, so a run can be re-read against a powermetrics log.
#
# The display dominates the total and must not move: run under `caffeinate -di`, brightness fixed,
# host otherwise quiet — and check for other people's VMs before starting.
set -uo pipefail
cd "$(dirname "$0")/../.."

disk=$1; mins=$2; shift 2
stamp=$(date +%H%M%S)
out=spikes/macos-timer-wakeup/battery-$stamp.txt
base=$(basename "$disk" .raw)

# mA (signed), mV, mAh. A discharge is reported as a two's-complement u64 — which is why this
# does not go through awk: 2^64 needs 64 bits of mantissa and awk has 53, so the subtraction
# silently returns 0 and every discharge reads as "charging".
sample() {
  ioreg -rn AppleSmartBattery | python3 -c "
import sys, re
t = sys.stdin.read()
def g(k):
    m = re.search(r'\"%s\" = (\d+)' % k, t)
    return int(m.group(1)) if m else 0
a = g('InstantAmperage')
if a > 2**63:
    a -= 2**64
print(a, g('Voltage'), g('AppleRawCurrentCapacity'))
"
}

block=1
for arm in "$@"; do
  sched=${arm%%:*}; load=${arm##*:}
  raw=spikes/macos-timer-wakeup/battery-$stamp-block$block.csv
  worker=/tmp/limina-worker-$base.log
  rm -f "$worker"

  if [ "$sched" = none ]; then
    echo "== block $block  no VM (host floor)" | tee -a "$out"
    sleep 30
    echo "start $(date +%H:%M:%S)" > "$raw"
    sum=0; n=0; end=$((SECONDS + mins * 60))
    while [ "$SECONDS" -lt "$end" ]; do
      sleep 5
      pmset -g ps | grep -q 'AC Power' && { echo "  PLUGGED IN — block $block invalid" | tee -a "$out"; break; }
      read -r a v c <<<"$(sample)"
      [ "$a" -ge 0 ] && continue
      echo "$(date +%H:%M:%S),$a,$v,$c" >> "$raw"
      sum=$((sum - a * v)); n=$((n + 1))
    done
    awk -v s="$sum" -v n="$n" -v f="$raw" -v m="$mins" 'BEGIN {
      if (n == 0) { print "  NO VALID SAMPLES"; exit }
      getline hdr < f
      c0 = ""; while ((getline line < f) > 0) { split(line, p, ","); if (c0 == "") c0 = p[4]; c1 = p[4] }
      printf "  mean %.2f W over %d samples, capacity %d -> %d mAh (%d in %d min)\n",
             s/n/1e6, n, c0, c1, c0-c1, m
    }' | tee -a "$out"
    block=$((block + 1)); continue
  fi

  LIMINA_DISK="$disk" LIMINA_NET=1 LIMINA_CPUS=8 RUST_LOG=limina=info,krun_vmm=info \
    LIMINA_VCPU_SCHED="$sched" \
    nohup spikes/venus-draw-probe/boot-enhanced-efi-kk.sh > "/tmp/limina-boot-$base.log" 2>&1 &
  boot=$!
  port=$(scripts/wait-guest-ssh.sh "$worker" 300 "$boot") || { echo "block $block: no ssh" | tee -a "$out"; continue; }
  pid=$(pgrep -f "^target/debug/limina-vmm" | head -1)
  ssh_guest() {
    ssh -p "$port" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
      -o ConnectTimeout=15 claude@127.0.0.1 "$@" 2>/dev/null
  }

  echo "== block $block  policy='$sched'  load=$load  worker $pid  ssh $port" | tee -a "$out"
  sleep 90   # GDM's first minute is login jobs, not idle

  if [ "$load" = vkcube ]; then
    ssh_guest "export XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0; \
               nohup vkcube --width 900 --height 600 >/tmp/vkcube.log 2>&1 & sleep 1"
    sleep 10
    ssh_guest "pgrep -a vkcube || tail -3 /tmp/vkcube.log" | sed 's/^/  vkcube: /' | tee -a "$out"
  fi

  # The differential has to be shown to reach the guest, per block — an unarmed B block and an
  # armed A block are both silent failures that look exactly like a null result.
  armed=$(grep -c 'VCPU-RT' "$worker" 2>/dev/null || echo 0)
  echo "  VCPU-RT lines: $armed" | tee -a "$out"
  grep -m3 'VCPU-RT' "$worker" 2>/dev/null | sed 's/^/    /' | tee -a "$out"

  echo "start $(date +%H:%M:%S)" > "$raw"
  sum=0; n=0
  end=$((SECONDS + mins * 60))
  while [ "$SECONDS" -lt "$end" ]; do
    sleep 5
    if pmset -g ps | grep -q 'AC Power'; then
      echo "  PLUGGED IN — block $block invalid" | tee -a "$out"; break
    fi
    read -r a v c <<<"$(sample)"
    # The gauge reports 0 or a positive current for a moment now and then; a single such sample is
    # meter noise, not a charge, and dropping it beats aborting a six-minute block.
    [ "$a" -ge 0 ] && continue
    echo "$(date +%H:%M:%S),$a,$v,$c" >> "$raw"
    sum=$((sum - a * v)); n=$((n + 1))
  done

  if [ "$load" = vkcube ]; then
    ssh_guest "pgrep -c vkcube" | sed 's/^/  vkcube alive at end: /' | tee -a "$out"
    # Watts per block say nothing until the frames they bought are counted: the banded arm is
    # meant to deliver ~50% more of them, and equal power would then be a win rather than a wash.
    if [ -n "${LIMINA_BATT_FPS:-}" ]; then
      scp -q -P "$port" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
        spikes/macos-timer-wakeup/fpsrun.sh claude@127.0.0.1:/tmp/ 2>/dev/null
      ssh_guest "chmod +x /tmp/fpsrun.sh; /tmp/fpsrun.sh block$block idle" \
        | sed 's/^/  FPS /' | tee -a "$out"
    fi
  fi
  top -l 4 -s 5 -stats pid,cpu,idlew -pid "$pid" | grep -E "^ *$pid " | sed 's/^/  worker /' | tee -a "$out"

  awk -v s="$sum" -v n="$n" -v f="$raw" -v m="$mins" 'BEGIN {
    if (n == 0) { print "  NO VALID SAMPLES"; exit }
    getline hdr < f
    c0 = ""; while ((getline line < f) > 0) { split(line, p, ","); if (c0 == "") c0 = p[4]; c1 = p[4] }
    printf "  mean %.2f W over %d samples, capacity %d -> %d mAh (%d in %d min)\n",
           s/n/1e6, n, c0, c1, c0-c1, m
  }' | tee -a "$out"

  ssh_guest "echo claudiusrobotus | sudo -S systemctl isolate multi-user.target"
  sleep 3
  ssh_guest "echo claudiusrobotus | sudo -S poweroff"
  for _ in $(seq 24); do pgrep -f "^target/debug/limina-vmm" >/dev/null || break; sleep 5; done
  pkill -f "^target/debug/limina --vmm-bin.*$base" 2>/dev/null
  wait "$boot" 2>/dev/null
  block=$((block + 1))
  sleep 10
done
echo "results in $out"
