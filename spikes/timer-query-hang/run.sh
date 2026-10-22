#!/bin/bash
# run.sh <disk.raw> <q|noq> <out-prefix> — one arm of the timer-query GPU-hang discrimination.
# Boots <disk.raw> (a clone of a guest with piglit built in ~/piglit: spikes/piglit-virgl/setup-guest.sh),
# swaps in the instrumented time-elapsed.c next to this script, rebuilds that one binary, and runs it
# once on gbm: `q` with its GL_TIME_ELAPSED queries, `noq` with every query removed (TQH_NOQ=1) and the
# same glFinish-timed doubling loop. Each draw prints `[TQH] iters=N cpu=S s` on stderr.
# Then runs it again capped at 64 iterations (TQH_MAX_ITERS) to see whether GL still renders: its
# last line is piglit's verdict on the rendered colour.
# Writes <out-prefix>.test.txt (the test's output), <out-prefix>.after.txt (the capped rerun),
# <out-prefix>.worker.log, <out-prefix>.gpurestart.txt and <out-prefix>.png (the window).
# A GPU hang is host-wide: run it only on a host with no other VM.
set -u
DISK=${1:?disk}; ARM=${2:?q|noq}; OUT=${3:?out prefix}
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(git -C "$HERE" rev-parse --show-toplevel)
cd "$ROOT"
LOG=/tmp/limina-worker-$(basename "${DISK%.raw}").log
SSH=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=error)
rm -f "$LOG"
start=$(date '+%Y-%m-%d %H:%M:%S')
LIMINA_DISK=$DISK LIMINA_CPUS=4 LIMINA_RAM_MIB=8192 LIMINA_WINDOW_CAPTURE="$OUT.png" \
  RUST_LOG=warn,limina=info,krun::vmm=info,krun_devices=info \
  spikes/venus-draw-probe/boot-enhanced-efi-kk.sh > "$OUT.boot.out" 2>&1 &
boot=$!
port=$(scripts/wait-guest-ssh.sh "$LOG" 400 "$boot") || { echo "boot failed"; kill "$boot"; exit 1; }
g() { ssh -p "$port" "${SSH[@]}" claude@127.0.0.1 "$@"; }
scp -q -P "$port" "${SSH[@]}" "$HERE/time-elapsed.c" claude@127.0.0.1:piglit/tests/spec/ext_timer_query/time-elapsed.c
g 'cd ~/piglit && ninja bin/ext_timer_query-time-elapsed > /tmp/tqh-ninja.log 2>&1 || cat /tmp/tqh-ninja.log' \
  > "$OUT.build.txt"
env=""; [ "$ARM" = noq ] && env="TQH_NOQ=1"
gtimeout --kill-after=10 300 ssh -p "$port" "${SSH[@]}" claude@127.0.0.1 \
  "cd ~/piglit && $env PIGLIT_PLATFORM=gbm timeout -k 5 240 bin/ext_timer_query-time-elapsed -auto -fbo 2>&1; echo rc=\$?" \
  > "$OUT.test.txt"
echo "test done: $(tail -1 "$OUT.test.txt")"
gtimeout --kill-after=10 120 ssh -p "$port" "${SSH[@]}" claude@127.0.0.1 \
  "cd ~/piglit && TQH_NOQ=1 TQH_MAX_ITERS=64 PIGLIT_PLATFORM=gbm timeout -k 5 90 bin/ext_timer_query-time-elapsed -auto -fbo 2>&1 | grep -v TQH; echo rc=\$?" \
  > "$OUT.after.txt"
echo "after: $(tr '\n' ' ' < "$OUT.after.txt")"
cp "$LOG" "$OUT.worker.log"
log show --start "$start" --predicate 'eventMessage CONTAINS "GPURestart" OR eventMessage CONTAINS "Deny submissions"' \
  > "$OUT.gpurestart.txt" 2>&1
g 'sudo systemctl poweroff' >/dev/null 2>&1
for _ in $(seq 1 40); do kill -0 "$boot" 2>/dev/null || break; sleep 3; done
if kill -0 "$boot" 2>/dev/null; then
  # A banned worker ignores SIGTERM; reap only the worker that boots this disk.
  pkill -9 -f "[l]imina-vmm.*--disk $DISK" ; kill "$boot" 2>/dev/null
fi
wait "$boot" 2>/dev/null
echo "worker log: $OUT.worker.log"
