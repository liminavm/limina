#!/bin/bash
# boot.sh <disk.raw> <worker-log> [bindir] — headless 4 vCPU / 4 GiB boot of a stock F44 clone on
# the M1 mini. bindir defaults to the installed release app's binaries (used read-only, never
# modified); pass <repo>/target/debug for the staged debug build. Software-2D
# display to a PNG (no host GPU work), no audio/USB/battery, so the VM's host side is as quiet as
# limina allows. The log is truncated, not removed, so a waiter never reads a stale port line.
set -u
DISK=${1:?disk}; LOG=${2:?log}; BIN=${3:-/Applications/Limina.app/Contents/MacOS}
FW=/Applications/Limina.app/Contents/Resources/KRUN_EFI.gop.fd
export PATH=/opt/homebrew/bin:$PATH
export LIMINA_WORKER_LAUNCH=spawn LIMINA_GVPROXY_BIN=/opt/homebrew/bin/gvproxy
export RUST_LOG=warn,limina=info,krun::vmm=info,krun_devices=info
: >"$LOG"
exec "$BIN/limina" --vmm-bin "$BIN/limina-vmm" --firmware "$FW" \
  --disk "$DISK" --discard-suspend --cpus 4 --ram-mib 4096 --net \
  --gpu-software-2d --display-capture "$(dirname "$LOG")/display.png" \
  --no-snd --no-usb --no-battery \
  >>"$LOG" 2>&1
