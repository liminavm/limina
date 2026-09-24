#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# One point of the flush-latency measurement: boot the enhanced image on a given prebuilt worker,
# play a VP9 clip in Firefox (VA-API -> virglrs -> VideoToolbox), and trace the guest's virtio-gpu
# control queue for TRACE_SECS. parse.py turns the trace into per-command latency.
#
# Usage: spikes/flush-latency/point.sh <label> <worker-dir> <decode-delay-ms> [video|idle]
#   <worker-dir> holds a `limina` and a codesigned `limina-vmm` (target/lat-sync, target/lat-async).
#   `idle` traces the same desktop with no video, as the floor both arms share.
set -uo pipefail
LABEL="${1:?label}"; WDIR="${2:?worker dir}"; DELAY="${3:?decode delay ms}"; MODE="${4:-video}"
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel); cd "$ROOT"
D=spikes/flush-latency
EV="$D/evidence/$LABEL"; WORK="$D/work.noindex"
mkdir -p "$EV" "$WORK"
TRACE_SECS="${TRACE_SECS:-30}"
log() { echo "[$(date +%H:%M:%S)] $*"; }
CLONE="$WORK/flush-enh.raw"
CLIP="$WORK/clip-720p30-vp9.webm"

[ -f "$CLIP" ] || ffmpeg -hide_banner -loglevel error -f lavfi \
  -i testsrc2=size=1280x720:rate=30:duration=150 \
  -c:v libvpx-vp9 -deadline realtime -cpu-used 8 -b:v 3M -y "$CLIP" || { log "ABORT: clip"; exit 1; }

rm -f "$WORK/flush-enh.raw"
cp -c Fedora-Workstation-44.enhanced.raw "$CLONE" || { log "ABORT: clone"; exit 1; }
{ echo "label=$LABEL worker=$WDIR delay=${DELAY}ms mode=$MODE"; ls -l "$WDIR"; } > "$EV/provenance.txt"

env LIMINA_BIN="$WDIR/limina" LIMINA_VMM_BIN="$WDIR/limina-vmm" \
  LIMINA_CPUS=4 LIMINA_RAM_MIB=4096 LIMINA_NET=1 LIMINA_EXTRA_ARGS="--display-resolution 1280x800" \
  RUST_LOG=warn,limina=info,krun_vmm=info,krun_devices=info \
  VIRGLRS_SUBMIT_STATS=2 VIRGLRS_DECODE_DELAY_MS="$DELAY" \
  LIMINA_DISK="$CLONE" spikes/venus-draw-probe/boot-enhanced-efi-kk.sh > "$EV/boot.txt" 2>&1 &
BPID=$!
WLOG=/tmp/limina-worker-flush-enh.log
PORT=$(scripts/wait-guest-ssh.sh "$WLOG" 400 "$BPID") || { log "ABORT: boot"; kill "$BPID"; exit 2; }
SSH=(ssh -p "$PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR claude@127.0.0.1)

# Settle: the same rule as the perf points -- 200 s of uptime and a quiet load average.
timeout 900 "${SSH[@]}" 'for i in $(seq 1 48); do
    L=$(cut -d" " -f1 /proc/loadavg); up=$(cut -d. -f1 /proc/uptime)
    if [ "$up" -ge 200 ]; then case "$L" in 0.0*|0.1*|0.2*) echo "SETTLED up=${up}s load=$L"; break;; esac; fi
    sleep 15
  done; uname -r; rpm -q mesa-dri-drivers' > "$EV/settle.txt" 2>&1
log "$LABEL: $(head -n 1 "$EV/settle.txt")"

if [ "$MODE" = video ]; then
  scp -P "$PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -q \
    "$CLIP" claude@127.0.0.1:/tmp/clip.webm
  "${SSH[@]}" "export XDG_RUNTIME_DIR=/run/user/1000
    systemd-run --user --unit=ff-clip --setenv=WAYLAND_DISPLAY=wayland-0 --setenv=MOZ_ENABLE_WAYLAND=1 \
      --setenv=XDG_RUNTIME_DIR=/run/user/1000 /usr/bin/firefox --kiosk file:///tmp/clip.webm >/dev/null"
  sleep 25
fi

# The trace itself. A 64 MiB buffer per CPU holds far more than 30 s of control-queue traffic.
timeout 120 "${SSH[@]}" "sudo sh -c 'cd /sys/kernel/tracing && echo 0 > tracing_on && echo > trace &&
    echo 65536 > buffer_size_kb &&
    echo 1 > events/virtio_gpu/virtio_gpu_cmd_queue/enable &&
    echo 1 > events/virtio_gpu/virtio_gpu_cmd_response/enable &&
    echo 1 > tracing_on && sleep $TRACE_SECS && echo 0 > tracing_on &&
    cat trace > /tmp/vgtrace.txt && cat per_cpu/cpu*/stats | grep overrun'" \
  > "$EV/trace-stats.txt" 2>&1
scp -P "$PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -q \
  claude@127.0.0.1:/tmp/vgtrace.txt "$EV/vgtrace.txt"
log "$LABEL: overruns $(awk '{s+=$2} END {print s}' "$EV/trace-stats.txt")"

"${SSH[@]}" 'systemctl --user stop ff-clip 2>/dev/null; sudo systemctl isolate multi-user.target' >/dev/null 2>&1
sleep 5
timeout 30 "${SSH[@]}" 'sudo systemctl poweroff' >/dev/null 2>&1
for _ in $(seq 1 60); do kill -0 "$BPID" 2>/dev/null || break; sleep 3; done
if kill -0 "$BPID" 2>/dev/null; then
  for p in $(pgrep -f "[l]imina.*--disk $CLONE"); do log "SIGKILL $(ps -o pid,command -p "$p" | tail -n 1)"; kill -9 "$p"; done
fi
cp "$WLOG" "$EV/worker.log"
grep -E 'vrend video:|DECODE_DELAY' "$EV/worker.log" > "$EV/video-lines.txt"
log "$LABEL: $(grep -c 'frames' "$EV/video-lines.txt") decode windows"
rm -f "$WORK/flush-enh.raw"
python3 "$D/parse.py" "$EV/vgtrace.txt" "$LABEL" | tee "$EV/latency.txt"
