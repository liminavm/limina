#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# One point of the 2026-10-07 list-restart A/B, run on ANOTHER Mac with a packaged Limina.app:
# the same arms and workload as point.sh (aquarium 25k/30k x2 on the vrend tier), driven from this
# Mac. The guest only trusts this Mac's ssh key, so guest steps go through the remote host
# (ssh -J); the boot, the shutdown and the frame dumps live there, and the fps crops are cut here.
#
# READING THIS PASS: both arms run the same app, KosmicKrisp and guest image, and differ only in
# LIMINA_KK_NOLISTRESTART, which KK reads in the worker (kk_cmd_draw.c requires_unroll_restart):
#   skip     unset: the default, list draws with restart enabled are NOT unrolled (not conformant)
#   unroll   =0: list draws with restart enabled are unrolled on the GPU (conformant)
# KK logs nothing about the variable, so each point proves its arm from the worker's environment.
#
# Usage: REMOTE=<user@host> RDIR=<dir on the remote holding Limina.app and the image> \
#          perf/listrestart-2026-10-07/point-remote.sh <label> <skip|unroll>
# The host name is passed in, never written here: this tree is public.
# EXTRA_ENV="K=V ..." adds worker environment for a diagnostic point (e.g. LIMINA_KK_STATS=1); its
# fps are then not comparable with a plain point's.
set -uo pipefail
LABEL="${1:?label}"; ARM="${2:?skip or unroll}"
REMOTE="${REMOTE:?REMOTE=<user@host>}"; RDIR="${RDIR:?RDIR=<remote dir>}"
case "$ARM" in
  skip) ARMV="" ;;
  unroll) ARMV="LIMINA_KK_NOLISTRESTART=0" ;;
  *) echo "arm must be skip or unroll" >&2; exit 1 ;;
esac
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel); cd "$ROOT"
DIR=perf/listrestart-2026-10-07
EV="$DIR/evidence/$LABEL"; LEDGER="$DIR/ledger.csv"
mkdir -p "$EV"
[ -f "$LEDGER" ] || echo "date,commit,workload,metric,value,notes" > "$LEDGER"
log() { echo "[$(date +%H:%M:%S)] $*"; }

PORT=2299
IMG=Fedora-Workstation-44.enhanced.raw
CLONE="$RDIR/ab-enh.raw"
CAP="$RDIR/aq-capture.png"
WLOG="$RDIR/worker-$LABEL.log"
SSHO=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o BatchMode=yes)
# The remote login shell may not be a POSIX one (fish rejects these), so every remote command
# runs under bash.
rsh() { ssh "${SSHO[@]}" "$REMOTE" bash -s <<< "$1"; }
SSH=(ssh "${SSHO[@]}" -J "$REMOTE" -p "$PORT" claude@127.0.0.1)
BUS='export XDG_RUNTIME_DIR=/run/user/1000 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus'

APPV=$(rsh "codesign -dvvv '$RDIR/Limina.app' 2>&1 | grep -m1 CDHash=")
NOTE="listrestart $LABEL: arm $ARM ($IMG) on a remote Mac; app $APPV; limina $(git rev-parse --short HEAD)"
echo "$NOTE" > "$EV/provenance.txt"
log "point $NOTE"

guest_up() { # waits for a real login, not just the banner
  for _ in $(seq 1 120); do
    timeout 15 "${SSH[@]}" true 2>/dev/null && return 0
    sleep 5
  done
  return 1
}
boot() {
  rsh "cd '$RDIR' && rm -f ab-enh.raw && cp -c '$IMG' ab-enh.raw && rm -f '$CAP' &&
    (env $ARMV ${EXTRA_ENV:-} LIMINA_WINDOW_CAPTURE='$CAP' RUST_LOG=warn,limina=info,krun::vmm=info,krun_devices=info \
      nohup ./Limina.app/Contents/MacOS/limina --disk '$CLONE' --cpus 4 --ram-mib 4096 --net \
        --ssh-port $PORT --window --display-resolution 1280x800 > '$WLOG' 2>&1 < /dev/null &)" || return 1
  guest_up
}
prove_arm() {
  rsh "for w in \$(pgrep -f '[l]imina-vmm.*ab-enh.raw'); do
      echo \"worker \$w LIMINA_KK_NOLISTRESTART=0 count: \$(ps -E -ww -o command= -p \$w | tr ' ' '\\n' | grep -c '^LIMINA_KK_NOLISTRESTART=0\$')\"
    done" > "$EV/arm-env.txt"
  log "arm $ARM: $(tr '\n' ' ' < "$EV/arm-env.txt")"
}
pin_display() {
  scp "${SSHO[@]}" -J "$REMOTE" -P "$PORT" -q scripts/perf/set-guest-display.py claude@127.0.0.1:/tmp/
  timeout 300 "${SSH[@]}" "$BUS; chmod +x /tmp/set-guest-display.py; sudo systemctl isolate graphical.target
    for i in \$(seq 1 60); do /tmp/set-guest-display.py --show >/dev/null 2>&1 && break; sleep 5; done
    /tmp/set-guest-display.py --write-config 1280x800 1.0 60" > "$EV/pin.txt" 2>&1 || return 1
  timeout 30 "${SSH[@]}" 'sudo systemctl reboot' >/dev/null 2>&1
  sleep 20
  guest_up || return 1
  scp "${SSHO[@]}" -J "$REMOTE" -P "$PORT" -q scripts/perf/set-guest-display.py claude@127.0.0.1:/tmp/
  timeout 300 "${SSH[@]}" "$BUS; chmod +x /tmp/set-guest-display.py; sudo systemctl isolate graphical.target
    for i in \$(seq 1 60); do /tmp/set-guest-display.py --show >/dev/null 2>&1 && break; sleep 5; done
    /tmp/set-guest-display.py --verify 1280x800 1.0 60" >> "$EV/pin.txt" 2>&1 || return 1
  log "display pinned: $(tail -n 1 "$EV/pin.txt")"
}
settle() {
  timeout 900 "${SSH[@]}" 'for i in $(seq 1 48); do
      L=$(cut -d" " -f1 /proc/loadavg); up=$(cut -d. -f1 /proc/uptime)
      if [ "$up" -ge 200 ]; then case "$L" in 0.0*|0.1*|0.2*) echo "SETTLED up=${up}s load=$L"; break;; esac; fi
      sleep 15
    done; echo "final: up=$(cut -d. -f1 /proc/uptime)s load=$(cut -d" " -f1-3 /proc/loadavg)"' > "$EV/settle.txt" 2>&1
  log "$(tail -n 1 "$EV/settle.txt")"
}
# One fish count: launch Firefox, settle, then fetch dumps until one is new and holds the counter.
aquarium() { # <run> <fish>
  local out="$EV/aquarium-$1"; mkdir -p "$out"
  timeout 120 "${SSH[@]}" "$BUS
    systemctl --user stop ff-bench 2>/dev/null || true
    systemctl --user reset-failed ff-bench 2>/dev/null || true
    sleep 3
    busctl --user set-property org.gnome.Shell /org/gnome/Shell org.gnome.Shell OverviewActive b false 2>/dev/null || true
    systemd-run --user --unit=ff-bench \
      --setenv=WAYLAND_DISPLAY=wayland-0 --setenv=MOZ_ENABLE_WAYLAND=1 \
      --setenv=MOZ_DISABLE_GPU_SANDBOX=1 --setenv=XDG_RUNTIME_DIR=/run/user/1000 \
      /usr/bin/firefox --kiosk 'https://webglsamples.org/aquarium/aquarium.html?numFish=$2' >/dev/null" || return 1
  sleep 35
  local full="$out/vrend-$2.png"
  for attempt in $(seq 1 12); do
    local before; before=$(rsh "stat -f %m '$CAP'")
    for _ in $(seq 1 30); do
      [ "$(rsh "stat -f %m '$CAP'")" -gt "$before" ] && break
      sleep 1
    done
    sleep 0.5
    scp "${SSHO[@]}" -q "$REMOTE:$CAP" "$full"
    if scripts/perf/crop-fps.py "$full" "$out/vrend-$2-fps.png" --require-content; then
      log "aquarium $1 $2: wrote $out/vrend-$2-fps.png"; return 0
    fi
    log "aquarium $1 $2: capture attempt $attempt unusable, retrying"
    sleep 5
  done
  log "aquarium $1 $2: FAILED to capture a usable frame"; return 1
}
shut() {
  timeout 60 "${SSH[@]}" "$BUS; systemctl --user stop ff-bench 2>/dev/null; sudo systemctl isolate multi-user.target" >/dev/null 2>&1
  sleep 5
  timeout 30 "${SSH[@]}" 'sudo systemctl poweroff' >/dev/null 2>&1
  for _ in $(seq 1 60); do
    rsh "pgrep -f '[l]imina.*ab-enh.raw' >/dev/null" || break
    sleep 3
  done
  if rsh "pgrep -f '[l]imina.*ab-enh.raw' >/dev/null"; then
    log "poweroff timed out; killing this point's VM by its disk"
    rsh "for p in \$(pgrep -f '[l]imina.*ab-enh.raw'); do ps -o pid,lstart,command -p \$p | tail -n 1; kill \$p; done"
    sleep 10
  fi
  scp "${SSHO[@]}" -q "$REMOTE:$WLOG" "$EV/worker.log" 2>/dev/null
  rsh "rm -f '$CLONE'"
}

log "boot"
boot || { log "ABORT: boot"; shut; exit 3; }
prove_arm
pin_display || { log "ABORT: display pin"; shut; exit 4; }
settle
for r in r1 r2; do
  for n in 25000 30000; do aquarium "$r" "$n"; done
done
shut
log "point $LABEL done; read $EV/aquarium-r*/*-fps.png"
