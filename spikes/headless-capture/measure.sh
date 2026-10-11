#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# What headless frame capture costs: boot one disk headless in one display mode, run `vkcube --c
# FRAMES` in its GNOME session RUNS times, and report the guest's fps and the CPU time the worker
# and the supervisor spent over each run.
#
#   spikes/headless-capture/measure.sh <mode> <disk clone> <out dir>
#
# <mode>: `png` (--display-capture, the headless display before --headless-capture existed),
# `gate` (--headless-capture, no capture running), `capture` (--headless-capture with a
# `capture start`/`stop` around every run) or `window` (a window, for reference). The disk boots in place: hand it a `cp -c` clone.
# Boots through spikes/venus-draw-probe/boot-enhanced-efi-kk.sh at 4 vCPU / 4096 MiB, 1280x800.
set -u
MODE=${1:?mode: png|gate|capture|window}
DISK=${2:?disk clone}
OUT=${3:?out dir}
FRAMES=${FRAMES:-600}
RUNS=${RUNS:-3}
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
cd "$ROOT"
mkdir -p "$OUT"
LOG="$OUT/worker.log"
: >"$LOG"
export LIMINA_DISK="$DISK" LIMINA_CPUS=4 LIMINA_RAM_MIB=4096 LIMINA_BOOT_LOG="$LOG"
export LIMINA_EXTRA_ARGS="--display-size 1280x800"
export RUST_LOG=warn,limina=info,krun::vmm=info,krun_devices=info
case "$MODE" in
  png) export LIMINA_DISPLAY_CAPTURE="$OUT/scanout.png" ;;
  gate|capture) export LIMINA_HEADLESS_CAPTURE=1 ;;
  window) export LIMINA_EXTRA_ARGS="--display-resolution 1280x800" ;;
  *) echo "unknown mode $MODE"; exit 2 ;;
esac
gtimeout --kill-after=10 1800 spikes/venus-draw-probe/boot-enhanced-efi-kk.sh >"$OUT/boot.out" 2>&1 &
BOOT=$!
PORT=$(scripts/wait-guest-ssh.sh "$LOG" 300 "$BOOT") || { echo "no ssh"; exit 1; }
SSH=(ssh -p "$PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes
     -o LogLevel=ERROR claude@127.0.0.1)
for _ in $(seq 120); do
  gtimeout 20 "${SSH[@]}" "sudo journalctl -b _COMM=gnome-shell --no-pager | grep -q 'GNOME Shell started'" && break
  sleep 2
done
sleep 20
SUP=$(ps -ww -o pid=,command= -A | awk -v d="$DISK" '/[t]arget\/debug\/limina --vmm-bin/ && index($0, d) {print $1; exit}')
WORKER=$(ps -ww -o pid=,command= -A | awk -v d="$DISK" '$2 ~ /limina-vmm$/ && index($0, d) && !/--launcher/ {print $1; exit}')
echo "mode=$MODE supervisor=$SUP worker=$WORKER port=$PORT" | tee "$OUT/result.txt"
cpu() { ps -o time= -p "$1" | awk -F'[:.]' '{ printf "%.2f\n", $1*60 + $2 + $3/100 }'; }
SESSION="env XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0"
for run in $(seq "$RUNS"); do
  [ "$MODE" = capture ] && target/debug/limina debug "$SUP" capture start "$OUT/cap$run" >>"$OUT/result.txt"
  s0=$(cpu "$SUP"); w0=$(cpu "$WORKER")
  el=$(gtimeout 120 "${SSH[@]}" "$SESSION python3 -c 'import subprocess, time; t = time.time(); \
subprocess.run([\"vkcube\", \"--c\", \"$FRAMES\"], capture_output=True); print(time.time() - t)'")
  s1=$(cpu "$SUP"); w1=$(cpu "$WORKER")
  [ "$MODE" = capture ] && target/debug/limina debug "$SUP" capture stop >>"$OUT/result.txt"
  awk -v m="$MODE" -v r="$run" -v f="$FRAMES" -v el="$el" -v s0="$s0" -v s1="$s1" -v w0="$w0" -v w1="$w1" \
    'BEGIN { printf "%s run %d: %.1f fps; worker %.0f%% CPU, supervisor %.1f%% CPU (%.1f s)\n",
             m, r, f/el, 100*(w1-w0)/el, 100*(s1-s0)/el, el }' | tee -a "$OUT/result.txt"
done
target/debug/limina stop "$SUP" --timeout 60 >>"$OUT/result.txt" 2>&1 \
  || target/debug/limina stop --force "$SUP" >>"$OUT/result.txt" 2>&1
wait "$BOOT"
