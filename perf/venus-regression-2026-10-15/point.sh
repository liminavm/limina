#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# One point of the venus-regression A/B: boot metal.test on venus with a PARKED BUNDLE's binaries,
# firmware, KosmicKrisp and zink-on-KK (nothing from this tree's target/ or /Volumes/mesa-cs), and
# run perf-ledger x1 (gl-replay, headless vk-replay, glmark2-wayland, the llvmpipe control) and
# vkmark x3, with GL through Fedora's zink on the venus ICD as in perf/macos27-2026-10-15. Rows go
# to this directory's ledger, never to perf/ledger.csv.
#
# The guest is the same image for every bundle, so a difference between points is the host stack.
#
# Usage: perf/venus-regression-2026-10-15/point.sh <label> <mounted Limina.app>
#   VCPU_SCHED=<policy>  passes LIMINA_VCPU_SCHED to the worker (read back from its log)
set -uo pipefail
LABEL="${1:?label}"; APP="${2:?path to a mounted Limina.app}"
[ -x "$APP/Contents/MacOS/limina-vmm" ] || { echo "no worker in $APP" >&2; exit 1; }
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel); cd "$ROOT"
DIR=perf/venus-regression-2026-10-15
WORK=$DIR/work.noindex
EV="$DIR/evidence/$LABEL"; LEDGER="$DIR/ledger.csv"
mkdir -p "$EV" "$WORK"
[ -f "$LEDGER" ] || echo "date,commit,workload,metric,value,notes" > "$LEDGER"
log() { echo "[$(date +%H:%M:%S)] $*"; }
IMG=Fedora-Workstation-44.metal.test.raw
GICD=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json
GENV="MESA_LOADER_DRIVER_OVERRIDE=zink GALLIUM_DRIVER=zink VK_DRIVER_FILES=$GICD"

# The bundle's own Mesa: a prefix whose lib/ is the bundle's Frameworks, so the boot vehicle's
# DYLD and LIBGL paths resolve inside the bundle. One prefix per bundle, named by its parked name.
BNAME=$(basename "$(dirname "$APP")")
PREFIX="$ROOT/$WORK/prefix-$BNAME"
mkdir -p "$PREFIX"; ln -sfn "$APP/Contents/Frameworks" "$PREFIX/lib"
ARM_ENV=(LIMINA_BIN="$APP/Contents/MacOS/limina" LIMINA_VMM_BIN="$APP/Contents/MacOS/limina-vmm"
  LIMINA_FIRMWARE="$APP/Contents/Resources/KRUN_EFI.gop.fd"
  LIMINA_KK_ICD="$APP/Contents/Resources/vulkan/kosmickrisp_icd.json" MESA_PREFIX="$PREFIX"
  MESA_SHADER_CACHE_DIR="$ROOT/$WORK/shader-cache-$BNAME"
  ${VCPU_SCHED:+LIMINA_VCPU_SCHED="$VCPU_SCHED"})
CLONE=$WORK/venus.raw
drop_clone() { rm -f perf/venus-regression-2026-10-15/work.noindex/venus.raw; }

OS=$(sw_vers -productVersion)
NOTE="venus-regression $LABEL: bundle $BNAME${VCPU_SCHED:+ sched $VCPU_SCHED} ($IMG); macOS $OS"
{ echo "$NOTE"; ls -l "$APP/Contents/MacOS/" "$APP/Contents/Frameworks/"; } > "$EV/provenance.txt"
log "point $NOTE"

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
keep_log() { cp "/tmp/limina-worker-$(basename "$CLONE" .raw).log" "$EV/worker.log" 2>/dev/null; }
SSHO=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR)
boot() { # -> PORT, BPID, SSH
  drop_clone; cp -c "$IMG" "$CLONE" || return 1
  env "${ARM_ENV[@]}" \
    LIMINA_CPUS=4 LIMINA_RAM_MIB=4096 LIMINA_NET=1 LIMINA_EXTRA_ARGS="--display-resolution 1280x800" \
    RUST_LOG=warn,limina=info,krun::vmm=info,krun_vmm=info,krun_devices=info \
    LIMINA_DISK="$CLONE" spikes/venus-draw-probe/boot-enhanced-efi-kk.sh > "$EV/boot.txt" 2>&1 &
  BPID=$!
  PORT=$(scripts/wait-guest-ssh.sh "/tmp/limina-worker-venus.log" 400 "$BPID") || return 1
  SSH=(ssh -p "$PORT" "${SSHO[@]}" claude@127.0.0.1)
  pin_display
}
# The display pin by a session restart (PIN=session in perf/macos27-2026-10-15/point.sh): the
# worker and guest kernel stay those of the first boot, which is what a bisect wants.
pin_display() {
  local bus='export XDG_RUNTIME_DIR=/run/user/1000 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus'
  scp -P "$PORT" "${SSHO[@]}" -q scripts/perf/set-guest-display.py claude@127.0.0.1:/tmp/
  timeout 300 "${SSH[@]}" "$bus; chmod +x /tmp/set-guest-display.py; sudo systemctl isolate graphical.target
    for i in \$(seq 1 60); do /tmp/set-guest-display.py --show >/dev/null 2>&1 && break; sleep 5; done
    /tmp/set-guest-display.py --write-config 1280x800 1.0 60" > "$EV/pin.txt" 2>&1 || return 1
  timeout 180 "${SSH[@]}" "$bus; sudo systemctl isolate multi-user.target; sleep 5
    sudo systemctl isolate graphical.target
    for i in \$(seq 1 60); do sleep 5; /tmp/set-guest-display.py --show >/dev/null 2>&1 && break; done
    /tmp/set-guest-display.py --verify 1280x800 1.0 60" >> "$EV/pin.txt" 2>&1 || return 1
  log "display pinned: $(tail -n 1 "$EV/pin.txt")"
}
settle() {
  timeout 900 "${SSH[@]}" 'for i in $(seq 1 48); do
      L=$(cut -d" " -f1 /proc/loadavg); up=$(cut -d. -f1 /proc/uptime)
      if [ "$up" -ge 200 ]; then case "$L" in 0.0*|0.1*|0.2*) echo "SETTLED up=${up}s load=$L"; break;; esac; fi
      sleep 15
    done; echo "final: up=$(cut -d. -f1 /proc/uptime)s load=$(cut -d" " -f1-3 /proc/loadavg)"
    uname -r; rpm -q mesa-dri-drivers mesa-vulkan-drivers' > "$EV/settle.txt" 2>&1
  cat "$EV/settle.txt"
}
shut() {
  timeout 60 "${SSH[@]}" 'sudo systemctl isolate multi-user.target' >/dev/null 2>&1; sleep 5
  timeout 30 "${SSH[@]}" 'sudo systemctl poweroff' >/dev/null 2>&1
  for _ in $(seq 1 60); do kill -0 "$BPID" 2>/dev/null || break; sleep 3; done
  if kill -0 "$BPID" 2>/dev/null; then
    log "poweroff timed out"; kill_by_disk "$CLONE"; wait "$BPID" 2>/dev/null
  fi
  drop_clone
}
row() { echo "$(date +%Y-%m-%d),$BNAME,$1,$2,$3,\"$NOTE; $4\"" >> "$LEDGER"; }
step() { # <timeout-secs> <cmd...>
  local t=$1; shift
  timeout "$t" "$@"; local rc=$?
  [ "$rc" = 124 ] && log "WEDGED: step timed out after ${t}s: $*"
  return "$rc"
}
# The host is shared: record what else was running, before and after.
host_load() { ps -Ao pcpu,rss,comm -r | sed -n 1,8p | cut -c1-110 > "$EV/host-$1.txt"; }

host_load before
boot || { log "ABORT: boot"; kill_by_disk "$CLONE"; keep_log; exit 3; }
settle
step 120 "${SSH[@]}" "export XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0 $GENV
  vulkaninfo --summary 2>&1 | grep -E 'deviceName|driverName'
  glmark2-es2-wayland -b build:duration=0.5 --size 128x128 2>&1 | grep -E 'GL_RENDERER|GL_VERSION'" > "$EV/arm.txt" 2>&1
log "arm: $(tr '\n' ' ' < "$EV/arm.txt")"
LIMINA_SSH_PORT="$PORT" LIMINA_PERF_LEDGER="$LEDGER" LIMINA_PERF_RATE=60 LIMINA_PERF_TRANSPORT=venus \
  step 900 scripts/perf-ledger.sh "$NOTE" > "$EV/ledger-run.txt" 2>&1
log "perf-ledger exit=$?"; grep -E 'ABORT|GUARD|^[0-9-]+,.*(gl-replay|vk-replay|glmark2)' "$EV/ledger-run.txt" | cut -d, -f3,5
for r in 1 2 3; do
  step 600 "${SSH[@]}" "export XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0 VK_DRIVER_FILES=$GICD; vkmark -s 1280x720" \
    > "$EV/vkmark-run$r.txt" 2>&1
  s=$(sed -n 's/.*vkmark Score: \([0-9]*\).*/\1/p' "$EV/vkmark-run$r.txt")
  log "vkmark run $r: ${s:-NO SCORE}"; [ -n "$s" ] && row vkmark-default-venus score "$s" "run $r/3"
done
host_load after
shut; keep_log
grep -iE 'vcpu.*(band|sched|polic|armed)' "$EV/worker.log" > "$EV/sched.txt"
log "worker: $(grep -c 'fences are ordered on queue' "$EV/worker.log") ring bindings, $(grep -c poisoned "$EV/worker.log") poisoned lines, $(wc -l < "$EV/sched.txt") vCPU-policy lines (sched.txt)"
log "point $LABEL done"
