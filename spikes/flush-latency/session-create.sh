#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Is building a VideoToolbox session paid once per worker, or on every playback? One boot of the
# stock image, then Showtime started and stopped several times: three plays of the 1080p clip
# (each a new player process, so a new VA context and codec), then one of the 720p clip (a new
# frame size). The decode thread's `create` phase and the queue waits are read back in order.
#
# PLAYS lists the plays as <resolution>:<idle seconds before it>, so a play after a long idle
# tells a per-process cost from one the hardware pays again after it has been quiet.
#
# Usage: spikes/flush-latency/session-create.sh <label> <worker-dir>
set -uo pipefail
LABEL="${1:?label}"; WDIR="${2:?worker dir}"
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel); cd "$ROOT"
D=spikes/flush-latency
EV="$D/evidence/$LABEL"; WORK="$D/work.noindex"
mkdir -p "$EV" "$WORK"
log() { echo "[$(date +%H:%M:%S)] $*"; }
CLONE="$WORK/flush-enh.raw"
for c in 1920x1080 1280x720; do
  [ -f "$WORK/clip-${c}p30-vp9.webm" ] || { log "ABORT: run point.sh once for the $c clip"; exit 1; }
done

rm -f spikes/flush-latency/work.noindex/flush-enh.raw
cp -c Fedora-Workstation-44.stock.test.raw "$CLONE" || { log "ABORT: clone"; exit 1; }
{ echo "label=$LABEL worker=$WDIR image=stock.test player=showtime"; ls -l "$WDIR"; } > "$EV/provenance.txt"

env LIMINA_BIN="$WDIR/limina" LIMINA_VMM_BIN="$WDIR/limina-vmm" \
  LIMINA_CPUS=4 LIMINA_RAM_MIB=4096 LIMINA_NET=1 LIMINA_EXTRA_ARGS="--display-resolution 1280x800" \
  RUST_LOG=warn,limina=info,krun_vmm=info,krun_devices=info VIRGLRS_SUBMIT_STATS=2 \
  LIMINA_DISK="$CLONE" spikes/venus-draw-probe/boot-enhanced-efi-kk.sh > "$EV/boot.txt" 2>&1 &
BPID=$!
WLOG=/tmp/limina-worker-flush-enh.log
PORT=$(scripts/wait-guest-ssh.sh "$WLOG" 400 "$BPID") || { log "ABORT: boot"; kill "$BPID"; exit 2; }
SSHO=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR)
SSH=(ssh -p "$PORT" "${SSHO[@]}" claude@127.0.0.1)

timeout 900 "${SSH[@]}" 'for i in $(seq 1 48); do
    L=$(cut -d" " -f1 /proc/loadavg); up=$(cut -d. -f1 /proc/uptime)
    if [ "$up" -ge 200 ]; then case "$L" in 0.0*|0.1*|0.2*) echo "SETTLED up=${up}s load=$L"; break;; esac; fi
    sleep 15
  done' > "$EV/settle.txt" 2>&1
log "$LABEL: $(head -n 1 "$EV/settle.txt")"
for c in 1920x1080 1280x720; do
  scp -P "$PORT" "${SSHO[@]}" -q "$WORK/clip-${c}p30-vp9.webm" "claude@127.0.0.1:/tmp/clip-$c.webm"
done

PLAYS="${PLAYS:-1920x1080:0 1920x1080:0 1920x1080:0 1280x720:0}"
echo "plays=$PLAYS" >> "$EV/provenance.txt"
play=0
for p in $PLAYS; do
  c="${p%%:*}"; idle="${p##*:}"
  play=$((play + 1))
  [ "$idle" -gt 0 ] && { log "$LABEL: idle ${idle}s before play $play"; sleep "$idle"; }
  "${SSH[@]}" "export XDG_RUNTIME_DIR=/run/user/1000
    systemd-run --user --unit=clip-$play --setenv=WAYLAND_DISPLAY=wayland-0 \
      --setenv=XDG_RUNTIME_DIR=/run/user/1000 /usr/bin/showtime /tmp/clip-$c.webm >/dev/null"
  sleep 15
  "${SSH[@]}" "systemctl --user stop clip-$play" >/dev/null 2>&1
  sleep 8
  log "$LABEL: play $play ($c) done"
done

"${SSH[@]}" 'sudo systemctl isolate multi-user.target' >/dev/null 2>&1
sleep 5
timeout 30 "${SSH[@]}" 'sudo systemctl poweroff' >/dev/null 2>&1
for _ in $(seq 1 60); do kill -0 "$BPID" 2>/dev/null || break; sleep 3; done
if kill -0 "$BPID" 2>/dev/null; then
  for p in $(pgrep -f "[l]imina.*--disk $CLONE"); do log "SIGKILL $(ps -o pid,command -p "$p" | tail -n 1)"; kill -9 "$p"; done
fi
cp "$WLOG" "$EV/worker.log"
grep -E 'create [1-9]|waited' "$EV/worker.log" > "$EV/creates.txt"
rm -f spikes/flush-latency/work.noindex/flush-enh.raw
cat "$EV/creates.txt"
