#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Reproduce the Basemark stall and catch it in the act. Boots the stock guest the way the perf
# passes do (perf/eager-barrier-2026-09-28/point.sh: EFI+venus, 4 vCPU / 4 GiB, 1280x800 pinned at
# 1.0 and 60 Hz), runs client-stall.sh -- RUNS suites in one Firefox session -- and when the client
# announces a stall, samples our own worker on the host twice and keeps the window capture and the
# worker log beside the guest's dumps.
#
# Usage: spikes/basemark-stall/loop.sh [boots] [stalls-wanted]
#   Stops after <stalls-wanted> stalls (default 3) or <boots> boots (default 6).
#   RUNS, STALL_S, GIVE_UP_S pass through to the client.
set -uo pipefail
BOOTS=${1:-6}; WANT=${2:-3}
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel); cd "$ROOT"
D=spikes/basemark-stall
mkdir -p "$D/work.noindex" "$D/runs"
log() { echo "[$(date +%H:%M:%S)] $*"; }
STOCK=spikes/basemark-stall/work.noindex/stall-stock.raw
drop_clones() { rm -f spikes/basemark-stall/work.noindex/stall-stock.raw; }
CAPTURE="$ROOT/$D/work.noindex/capture.png"

V=$(git -C third_party/virglrs rev-parse --short HEAD); L=$(git -C third_party/libkrun rev-parse --short HEAD)
M=$(git -C /Volumes/mesa-cs/mesa rev-parse --short HEAD)
log "host mesa $M; virglrs $V + libkrun $L; limina $(git rev-parse --short HEAD)"
cargo xtask build > "$D/work.noindex/build.log" 2>&1 || { log "ABORT: build failed"; exit 2; }

kill_by_disk() { # <clone>
  for p in $(pgrep -f "[l]imina.*--disk $1"); do
    log "killing our own VM: $(ps -o pid,lstart,command -p "$p" | tail -n 1)"
    kill "$p"
  done
  for _ in $(seq 1 20); do pgrep -f "[l]imina.*--disk $1" >/dev/null || return 0; sleep 1; done
  for p in $(pgrep -f "[l]imina.*--disk $1"); do
    log "still alive after SIGTERM, SIGKILL: $(ps -o pid,lstart,command -p "$p" | tail -n 1)"
    kill -9 "$p"
  done
}
# Our worker is the process holding our clone open. It is started by launchd, so its argv is not
# a reliable handle; the open disk is, and no other session's VM can hold this path.
our_worker() { lsof -t "$ROOT/$STOCK" 2>/dev/null | while read -r p; do
  ps -o command= -p "$p" | grep -q '[l]imina-vmm' && echo "$p"; done | head -n 1; }

SSHO=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR)
boot() { # <clone> <source-image> -> sets PORT, BPID, SSH
  drop_clones; cp -c "$2" "$1" || return 1
  local tag; tag=$(basename "$1" .raw)
  env LIMINA_ZINK_RP_STATS=1 VIRGLRS_SUBMIT_STATS=1 \
    LIMINA_CPUS=4 LIMINA_RAM_MIB=4096 LIMINA_NET=1 LIMINA_EXTRA_ARGS="--display-resolution 1280x800" \
    RUST_LOG=warn,limina=info,krun_vmm=info,krun_devices=info LIMINA_WINDOW_CAPTURE="$CAPTURE" \
    LIMINA_DISK="$1" spikes/venus-draw-probe/boot-enhanced-efi-kk.sh > "$EV/boot.txt" 2>&1 &
  BPID=$!
  PORT=$(scripts/wait-guest-ssh.sh "/tmp/limina-worker-$tag.log" 400 "$BPID") || return 1
  SSH=(ssh -p "$PORT" "${SSHO[@]}" claude@127.0.0.1)
  pin_display "$tag"
}
# As point.sh: monitors.xml, a reboot (the supervisor relaunches the worker), verify.
pin_display() { # <tag>
  local bus='export XDG_RUNTIME_DIR=/run/user/1000 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus'
  scp -P "$PORT" "${SSHO[@]}" -q scripts/perf/set-guest-display.py claude@127.0.0.1:/tmp/
  timeout 300 "${SSH[@]}" "$bus; chmod +x /tmp/set-guest-display.py; sudo systemctl isolate graphical.target
    for i in \$(seq 1 60); do /tmp/set-guest-display.py --show >/dev/null 2>&1 && break; sleep 5; done
    /tmp/set-guest-display.py --write-config 1280x800 1.0 60" > "$EV/pin.txt" 2>&1 || return 1
  timeout 30 "${SSH[@]}" 'sudo systemctl reboot' >/dev/null 2>&1
  for _ in $(seq 1 60); do
    ssh -p "$PORT" "${SSHO[@]}" -o BatchMode=yes -o ConnectTimeout=3 claude@127.0.0.1 true 2>/dev/null || break
    sleep 2
  done
  scripts/wait-guest-ssh.sh "/tmp/limina-worker-$1.log" 400 "$BPID" >/dev/null || return 1
  scp -P "$PORT" "${SSHO[@]}" -q scripts/perf/set-guest-display.py claude@127.0.0.1:/tmp/
  timeout 300 "${SSH[@]}" "$bus; chmod +x /tmp/set-guest-display.py; sudo systemctl isolate graphical.target
    for i in \$(seq 1 60); do /tmp/set-guest-display.py --show >/dev/null 2>&1 && break; sleep 5; done
    /tmp/set-guest-display.py --verify 1280x800 1.0 60" >> "$EV/pin.txt" 2>&1 || return 1
  log "display pinned: $(tail -n 1 "$EV/pin.txt")"
}
settle() {
  timeout 900 "${SSH[@]}" 'for i in $(seq 1 48); do
      L=$(cut -d" " -f1 /proc/loadavg); up=$(cut -d. -f1 /proc/uptime)
      if [ "$up" -ge 200 ]; then case "$L" in 0.0*|0.1*|0.2*) echo "SETTLED up=${up}s load=$L"; break;; esac; fi
      sleep 15
    done; echo "final: up=$(cut -d. -f1 /proc/uptime)s load=$(cut -d" " -f1-3 /proc/loadavg)"
    uname -r; rpm -q mesa-dri-drivers firefox' > "$EV/settle.txt" 2>&1
  log "$(head -n 1 "$EV/settle.txt")"
}
shut() {
  timeout 60 "${SSH[@]}" 'sudo systemctl isolate multi-user.target' >/dev/null 2>&1; sleep 5
  timeout 30 "${SSH[@]}" 'sudo systemctl poweroff' >/dev/null 2>&1
  for _ in $(seq 1 60); do kill -0 "$BPID" 2>/dev/null || break; sleep 3; done
  if kill -0 "$BPID" 2>/dev/null; then log "poweroff timed out"; kill_by_disk "$STOCK"; wait "$BPID" 2>/dev/null; fi
  drop_clones
}

# The host half of a stall: two 10 s samples of our worker a minute apart (matching the guest's
# dumps a and b), the frame on screen, and the worker log so far.
host_dump() { # <run>
  local w; w=$(our_worker)
  local wl; wl="/tmp/limina-worker-$(basename "$STOCK" .raw).log"
  log "STALL in run $1: sampling worker ${w:-NONE}; worker log at byte $(wc -c < "$wl"), $(date +%s)"
  cp "$CAPTURE" "$EV/stall-$1-capture-a.png" 2>/dev/null
  [ -n "$w" ] && sample "$w" 10 -file "$EV/stall-$1-sample-a.txt" > /dev/null 2>&1
  sleep 50
  [ -n "$w" ] && sample "$w" 10 -file "$EV/stall-$1-sample-b.txt" > /dev/null 2>&1
  cp "$CAPTURE" "$EV/stall-$1-capture-b.png" 2>/dev/null
  cp "/tmp/limina-worker-$(basename "$STOCK" .raw).log" "$EV/worker-at-stall-$1.log" 2>/dev/null
}
# A reference sample of the same test running normally, taken once per boot.
healthy_sample() {
  local w; w=$(our_worker)
  [ -n "$w" ] && sample "$w" 5 -file "$EV/healthy-shader-pipeline-sample.txt" > /dev/null 2>&1
  log "reference sample of a running shader pipeline test taken"
}

stalls=0
for b in $(seq 1 "$BOOTS"); do
  EV="$D/runs/$(date +%Y%m%d-%H%M%S)-boot$b"; mkdir -p "$EV"
  log "boot $b -> $EV"
  boot "$STOCK" Fedora-Workstation-44.stock.test.raw ||
    { log "ABORT boot $b"; kill_by_disk "$STOCK"; wait "$BPID" 2>/dev/null; drop_clones; continue; }
  scp -P "$PORT" "${SSHO[@]}" -q $D/client-stall.sh $D/dump-guest.sh $D/marionette.py $D/tap-keys.py claude@127.0.0.1:/tmp/
  "${SSH[@]}" 'chmod +x /tmp/client-stall.sh /tmp/dump-guest.sh; sudo systemctl isolate graphical.target'
  settle
  RUNS=${RUNS:-4}
  timeout $((RUNS * 1500 + 300)) "${SSH[@]}" \
    "RUNS=$RUNS STALL_S=${STALL_S:-60} GIVE_UP_S=${GIVE_UP_S:-600} /tmp/client-stall.sh" > "$EV/client.txt" 2>&1 &
  CPID=$!
  # Follow the client's output and act on its announcements.
  seen=0; ref=0
  while kill -0 "$CPID" 2>/dev/null; do
    # A healthy test lasts 10-20 s: sample the moment the page shows, for 5 s, or it is the next one.
    if [ "$ref" = 0 ] && grep -q 'PATH .*shader_pipeline_test' "$EV/client.txt"; then ref=1; healthy_sample; fi
    n=$(grep -c '=== STALL run' "$EV/client.txt")
    while [ "$seen" -lt "$n" ]; do
      seen=$((seen + 1))
      host_dump "$(grep '=== STALL run' "$EV/client.txt" | sed -n "${seen}p" | awk '{for (i=1;i<NF;i++) if ($i=="run") {print $(i+1); exit}}')"
    done
    sleep 5
  done
  wait "$CPID"; log "client exit=$?"
  grep -E '=== (STALL|RECOVERED)|never reached|scores \(run|renderer=|REFUS' "$EV/client.txt"
  timeout 120 "${SSH[@]}" 'cd /tmp && ls -d stall-* >/dev/null 2>&1 && tar czf - stall-*' > "$EV/guest-dumps.tgz" 2>/dev/null
  [ -s "$EV/guest-dumps.tgz" ] && (cd "$EV" && tar xzf guest-dumps.tgz && rm guest-dumps.tgz) || rm -f "$EV/guest-dumps.tgz"
  shut
  cp "/tmp/limina-worker-$(basename "$STOCK" .raw).log" "$EV/worker.log" 2>/dev/null
  stalls=$((stalls + seen))
  log "boot $b done: $seen stall(s) this boot, $stalls total"
  [ "$stalls" -ge "$WANT" ] && break
done
log "loop done: $stalls stall(s)"
