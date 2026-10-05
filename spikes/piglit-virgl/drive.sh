#!/bin/bash
# drive.sh <disk.raw> <out-dir> — run piglit (run.sh) in a limina guest to completion, surviving
# host crashes. A test that aborts the worker takes the VM down; this keeps the worker log of each
# crash as <out-dir>/crash-<n>.log, boots the disk again, and resumes with --no-retry, so the
# killer is recorded as incomplete and the run moves on. Ends by copying the results out and
# powering the guest off. The guest must already have piglit built (setup-guest.sh) and run.sh.
set -u
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
cd "$ROOT"
DISK=${1:?disk}
OUT=${2:?out dir}
mkdir -p "$OUT"
LOG=/tmp/limina-worker-$(basename "${DISK%.raw}").log
SSH=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=error)
RES=results-gbm
n=0
while :; do
  LIMINA_DISK=$DISK LIMINA_CPUS=4 LIMINA_RAM_MIB=8192 \
    RUST_LOG=warn,limina=info,krun::vmm=info,krun_devices=info \
    LIMINA_WINDOW_CAPTURE=$OUT/window.png \
    spikes/venus-draw-probe/boot-enhanced-efi-kk.sh > "$OUT/boot-$n.out" 2>&1 &
  boot=$!
  port=$(scripts/wait-guest-ssh.sh "$LOG" 300 "$boot") || { echo "boot $n failed"; exit 1; }
  echo "boot $n: ssh port $port"
  scp -q -P "$port" "${SSH[@]}" spikes/piglit-virgl/run.sh claude@127.0.0.1:
  if [ ! -e "$OUT/.started" ]; then
    ssh -p "$port" "${SSH[@]}" claude@127.0.0.1 "rm -rf ~/results-gbm ~/results-gbm.log" &&
      touch "$OUT/.started"
  fi
  ssh -p "$port" "${SSH[@]}" claude@127.0.0.1 \
    "cd ~/piglit; if [ -d ~/$RES ]; then PIGLIT_PLATFORM=gbm ./piglit resume --no-retry ~/$RES >> ~/$RES.log 2>&1; else ~/run.sh ~/$RES; fi; echo piglit-exit=\$?"
  rc=$?
  if [ $rc -eq 0 ]; then
    ssh -p "$port" "${SSH[@]}" claude@127.0.0.1 "cd ~ && tar czf - $RES $RES.log" > "$OUT/results.tar.gz"
    cp "$LOG" "$OUT/worker-final.log"
    ssh -p "$port" "${SSH[@]}" claude@127.0.0.1 "sudo systemctl poweroff" || true
    wait "$boot"
    echo "done after $n crash(es)"
    exit 0
  fi
  # ssh dropped: the VM went down under a test. Keep the evidence before the next boot rm's it.
  wait "$boot"
  cp "$LOG" "$OUT/crash-$n.log"
  echo "boot $n: VM went down (ssh rc=$rc), worker log kept as crash-$n.log"
  n=$((n + 1))
  [ $n -lt 40 ] || { echo "giving up after $n crashes"; exit 1; }
done
