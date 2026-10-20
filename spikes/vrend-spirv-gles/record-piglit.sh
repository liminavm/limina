#!/bin/bash
# record-piglit.sh <disk.raw> <out-dir> — record vrend traces of piglit's feature slices on the
# Rust leg, for vrend's shader corpus (docs/design/vrend-spirv.md).
#
# Boots <disk> (piglit built in tree by spikes/piglit-virgl/setup-guest.sh) on the EFI+KK vehicle
# with the worker's vrend recorder armed, runs each group of piglit-groups.txt, and after each
# asks the recorder for a dump and keeps it as <out-dir>/<group>.bin with the group's piglit
# results beside it. Needs a worker built with the recorder:
#   cargo build -p limina -p limina-vmm --features virglrs/trace && crates/limina-vmm/sign.sh debug
# The recorder is a ring that evicts its oldest records and a dump does not empty it, so each dump
# holds that group's contexts whole (piglit runs one process, one context, per test) and possibly
# the tail of the groups before it; replay a group by its contexts.
set -u
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
cd "$ROOT"
DISK=${1:?disk}
OUT=$(cd "${2:?out dir}" && pwd)
GROUPS_FILE=$ROOT/spikes/vrend-spirv-gles/piglit-groups.txt
LOG=$OUT/worker.log
SSH=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=error)
TRACE=$OUT/trace.bin
FIFO=$OUT/trace.fifo

rm -f "$LOG" "$TRACE"
LIMINA_DISK=$DISK LIMINA_CPUS=4 LIMINA_RAM_MIB=8192 LIMINA_BOOT_LOG=$LOG \
  RUST_LOG=warn,limina=info,krun::vmm=info,krun_devices=info,virglrs=info \
  LIMINA_WINDOW_CAPTURE=$OUT/window.png \
  LIMINA_VREND_TRACE=${LIMINA_VREND_TRACE:-1024} LIMINA_VREND_TRACE_OUT=$TRACE \
  LIMINA_VREND_TRACE_FIFO=$FIFO \
  spikes/venus-draw-probe/boot-enhanced-efi-kk.sh > "$OUT/boot.out" 2>&1 &
boot=$!
port=$(scripts/wait-guest-ssh.sh "$LOG" 300 "$boot") || { echo "boot failed, see $LOG"; exit 1; }
echo "ssh port $port, worker log $LOG"
grep -q "trace:" "$LOG" || echo "warning: no recorder line in the worker log yet"

grep -v '^#' "$GROUPS_FILE" | while IFS=$'\t' read -r group re; do
  [ -n "$group" ] || continue
  echo "== $group: $re"
  ssh -n -p "$port" "${SSH[@]}" claude@127.0.0.1 \
    "cd ~/piglit && rm -rf ~/res-$group && PIGLIT_PLATFORM=gbm ./piglit run quick -1 -s --timeout 120 -t '$re' ~/res-$group > ~/res-$group.log 2>&1; echo piglit-exit=\$?; tail -n 3 ~/res-$group.log"
  rc=$?
  if [ $rc -eq 255 ]; then
    echo "$group: ssh dropped, the VM went down; worker log kept at $LOG"
    exit 1
  fi
  before=$(grep -c "trace: dumped" "$LOG")
  echo x > "$FIFO"
  for _ in $(seq 120); do
    [ "$(grep -c "trace: dumped" "$LOG")" -gt "$before" ] && break
    sleep 1
  done
  grep "trace: dumped" "$LOG" | tail -n 1
  mv "$TRACE" "$OUT/$group.bin"
  ssh -n -p "$port" "${SSH[@]}" claude@127.0.0.1 "cd ~ && tar czf - res-$group res-$group.log" \
    > "$OUT/$group.results.tar.gz"
done

ssh -n -p "$port" "${SSH[@]}" claude@127.0.0.1 "sudo systemctl poweroff" || true
wait "$boot"
echo "done; traces in $OUT"
