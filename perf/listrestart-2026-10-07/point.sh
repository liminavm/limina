#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# One point of the 2026-10-07 list-restart A/B: boot the enhanced guest on the shared build-kk and
# run aquarium 25k/30k x2 (the vrend reading, the workload the shortcut was added for). Adapted from
# perf/tier0-2026-10-05/point.sh; rows go to this directory's ledger, never to perf/ledger.csv.
# Aquarium fps is only drawn on screen: read the *-fps.png crops afterwards.
#
# READING THIS PASS: both arms run the same binaries, host mesa and guest image, and differ only in
# LIMINA_KK_NOLISTRESTART, which KK reads in the worker (kk_cmd_draw.c requires_unroll_restart):
#   skip     unset: the default, list draws with restart enabled are NOT unrolled (not conformant)
#   unroll   =0: list draws with restart enabled are unrolled on the GPU (conformant)
# KK logs nothing about the variable, so each point proves its arm from the worker's environment.
# It does NOT build: another session may be running the suite, and a build relinks the binaries
# under it. provenance.txt records the binaries it ran.
#
# Usage: perf/listrestart-2026-10-07/point.sh <label> <skip|unroll>
set -uo pipefail
LABEL="${1:?label}"; ARM="${2:?skip or unroll}"
CACHE="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)/perf/listrestart-2026-10-07/work.noindex/shader-cache-$ARM"
IMG=Fedora-Workstation-44.enhanced.raw
case "$ARM" in
  skip) ARMV=() ;;
  unroll) ARMV=(LIMINA_KK_NOLISTRESTART=0) ;;
  *) echo "arm must be skip or unroll" >&2; exit 1 ;;
esac
ICD=/Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.aarch64.json
ARM_ENV=(LIMINA_KK_ICD="$ICD" MESA_SHADER_CACHE_DIR="$CACHE" ${ARMV[@]+"${ARMV[@]}"})
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel); cd "$ROOT"
DIR=perf/listrestart-2026-10-07
EV="$DIR/evidence/$LABEL"; LEDGER="$DIR/ledger.csv"
mkdir -p "$EV" $DIR/work.noindex
[ -f "$LEDGER" ] || echo "date,commit,workload,metric,value,notes" > "$LEDGER"
log() { echo "[$(date +%H:%M:%S)] $*"; }
ENH=perf/listrestart-2026-10-07/work.noindex/trend-enh.raw
drop_clones() { rm -f perf/listrestart-2026-10-07/work.noindex/trend-enh.raw; }

V=$(git -C third_party/virglrs rev-parse --short HEAD); L=$(git -C third_party/libkrun rev-parse --short HEAD)
M=$(git -C /Volumes/mesa-cs/mesa rev-parse --short HEAD)
NOTE="listrestart $LABEL: arm $ARM ($IMG); host mesa $M; virglrs $V + libkrun $L; limina $(git rev-parse --short HEAD)"
{ echo "$NOTE"; ls -l target/debug/limina target/debug/limina-vmm /Volumes/mesa-cs/zink-kk-prefix/lib/libgallium*.dylib /Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/libvulkan_kosmickrisp.dylib;
  git -C /Volumes/mesa-cs/mesa status --short src/kosmickrisp; } > "$EV/provenance.txt"
log "point $NOTE"
SSHO=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR)
POISON="$EV/POISONED"
kill_by_disk() { # <clone>
  for p in $(pgrep -f "[l]imina.*--disk $1"); do   # the bracket keeps pgrep off this shell's argv
    log "killing our own VM: $(ps -o pid,lstart,command -p "$p" | tail -n 1)"
    kill "$p"
  done
  # A stalled guest's worker has been seen to ignore SIGTERM; escalate rather than block the sweep.
  for _ in $(seq 1 20); do pgrep -f "[l]imina.*--disk $1" >/dev/null || return 0; sleep 1; done
  for p in $(pgrep -f "[l]imina.*--disk $1"); do
    log "still alive after SIGTERM, SIGKILL: $(ps -o pid,lstart,command -p "$p" | tail -n 1)"
    kill -9 "$p"
  done
}
keep_log() { # <clone> <tag>
  local wl; wl="/tmp/limina-worker-$(basename "$1" .raw).log"
  cp "$wl" "$EV/worker-$2.log" 2>/dev/null
  log "worker-$2: $(grep -c poisoned "$EV/worker-$2.log" 2>/dev/null || true) poisoned-context lines"
}
boot() { # <clone> <source-image> [capture] -> sets PORT, BPID, SSH
  drop_clones; cp -c "$2" "$1" || return 1
  local tag; tag=$(basename "$1" .raw)
  env ${ARM_ENV[@]+"${ARM_ENV[@]}"} \
    LIMINA_CPUS=4 LIMINA_RAM_MIB=4096 LIMINA_NET=1 LIMINA_EXTRA_ARGS="--display-resolution 1280x800" \
    RUST_LOG=warn,limina=info,krun::vmm=info,krun_vmm=info,krun_devices=info ${3:+LIMINA_WINDOW_CAPTURE="$3"} \
    LIMINA_DISK="$1" spikes/venus-draw-probe/boot-enhanced-efi-kk.sh > "$EV/boot-$tag.txt" 2>&1 &
  BPID=$!
  PORT=$(scripts/wait-guest-ssh.sh "/tmp/limina-worker-$tag.log" 400 "$BPID") || return 1
  SSH=(ssh -p "$PORT" "${SSHO[@]}" claude@127.0.0.1)
  pin_display "$tag"
}
pin_display() { # <tag>
  local bus='export XDG_RUNTIME_DIR=/run/user/1000 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus'
  scp -P "$PORT" "${SSHO[@]}" -q scripts/perf/set-guest-display.py claude@127.0.0.1:/tmp/
  # The stock test image boots to multi-user; the seated session is what reads monitors.xml.
  timeout 300 "${SSH[@]}" "$bus; chmod +x /tmp/set-guest-display.py; sudo systemctl isolate graphical.target
    for i in \$(seq 1 60); do /tmp/set-guest-display.py --show >/dev/null 2>&1 && break; sleep 5; done
    /tmp/set-guest-display.py --write-config 1280x800 1.0 60" > "$EV/pin-$1.txt" 2>&1 || return 1
  if [ "${PIN:-reboot}" = session ]; then
    # Restart only the seated session: the worker, and the guest kernel, stay those of the first boot.
    timeout 120 "${SSH[@]}" "$bus; sudo systemctl isolate multi-user.target; sleep 5
      sudo systemctl isolate graphical.target
      for i in \$(seq 1 60); do sleep 5; /tmp/set-guest-display.py --show >/dev/null 2>&1 && break; done
      /tmp/set-guest-display.py --verify 1280x800 1.0 60" >> "$EV/pin-$1.txt" 2>&1 || return 1
    log "display pinned on $1 by a session restart: $(tail -n 1 "$EV/pin-$1.txt")"
    return 0
  fi
  timeout 30 "${SSH[@]}" 'sudo systemctl reboot' >/dev/null 2>&1
  # Down first, so the login the waiter tests is the new boot's and not the old one's.
  for _ in $(seq 1 60); do
    ssh -p "$PORT" "${SSHO[@]}" -o BatchMode=yes -o ConnectTimeout=3 claude@127.0.0.1 true 2>/dev/null || break
    sleep 2
  done
  scripts/wait-guest-ssh.sh "/tmp/limina-worker-$1.log" 400 "$BPID" >/dev/null || return 1
  # /tmp is a tmpfs, so the reboot took the script with it.
  scp -P "$PORT" "${SSHO[@]}" -q scripts/perf/set-guest-display.py claude@127.0.0.1:/tmp/
  timeout 300 "${SSH[@]}" "$bus; chmod +x /tmp/set-guest-display.py; sudo systemctl isolate graphical.target
    for i in \$(seq 1 60); do /tmp/set-guest-display.py --show >/dev/null 2>&1 && break; sleep 5; done
    /tmp/set-guest-display.py --verify 1280x800 1.0 60" >> "$EV/pin-$1.txt" 2>&1 || return 1
  log "display pinned on $1: $(tail -n 1 "$EV/pin-$1.txt")"
}
settle() { # <tag>
  timeout 900 "${SSH[@]}" 'for i in $(seq 1 48); do
      L=$(cut -d" " -f1 /proc/loadavg); up=$(cut -d. -f1 /proc/uptime)
      if [ "$up" -ge 200 ]; then case "$L" in 0.0*|0.1*|0.2*) echo "SETTLED up=${up}s load=$L"; break;; esac; fi
      sleep 15
    done; echo "final: up=$(cut -d. -f1 /proc/uptime)s load=$(cut -d" " -f1-3 /proc/loadavg)"
    uname -r; rpm -q mesa-dri-drivers' > "$EV/settle-$1.txt" 2>&1
  cat "$EV/settle-$1.txt"
}
shut() { # <clone>
  timeout 60 "${SSH[@]}" 'sudo systemctl isolate multi-user.target' >/dev/null 2>&1; sleep 5
  timeout 30 "${SSH[@]}" 'sudo systemctl poweroff' >/dev/null 2>&1
  for _ in $(seq 1 60); do kill -0 "$BPID" 2>/dev/null || break; sleep 3; done
  if kill -0 "$BPID" 2>/dev/null; then
    log "poweroff timed out"; kill_by_disk "$1"; wait "$BPID" 2>/dev/null
  fi
  drop_clones
}
row() { echo "$(date +%Y-%m-%d),$(git rev-parse --short HEAD),$1,$2,$3,\"$NOTE; $4\"" >> "$LEDGER"; }

# WATCHDOG. A venus client poisoning the compositor's vrend context makes every number taken
# afterwards not a measurement. A background watch on the worker log drops a POISONED marker the
# moment the first refusal lands; every row written after it says INVALID. Every guest step also
# runs under a timeout, so a wedged guest costs its point and never strands the sweep.
POISON="$EV/POISONED"
watch_start() { # <clone>
  local wl; wl="/tmp/limina-worker-$(basename "$1" .raw).log"
  ( while :; do
      if grep -q 'context is poisoned' "$wl" 2>/dev/null && [ ! -f "$POISON" ]; then
        echo "$(date +%H:%M:%S) $(basename "$1" .raw): $(grep -m1 'refused: vrend' "$wl")" > "$POISON"
        log "WATCHDOG: compositor context POISONED on $(basename "$1" .raw); later rows are INVALID"
      fi
      sleep 10
    done ) &
  WATCH=$!
}
watch_start() { # <clone>
  local wl; wl="/tmp/limina-worker-$(basename "$1" .raw).log"
  ( while :; do
      if grep -q 'context is poisoned' "$wl" 2>/dev/null && [ ! -f "$POISON" ]; then
        echo "$(date +%H:%M:%S) $(basename "$1" .raw): $(grep -m1 'refused: vrend' "$wl")" > "$POISON"
        log "WATCHDOG: compositor context POISONED on $(basename "$1" .raw); later rows are INVALID"
      fi
      sleep 10
    done ) &
  WATCH=$!
}
watch_stop() { kill "$WATCH" 2>/dev/null; wait "$WATCH" 2>/dev/null; }
valid() { [ -f "$POISON" ] && echo "INVALID: compositor context poisoned at $(cut -d' ' -f1 "$POISON")"; }
step() { # <timeout-secs> <cmd...>: run a guest step; a timeout is logged as WEDGED
  local t=$1; shift
  timeout "$t" "$@"; local rc=$?
  [ "$rc" = 124 ] && log "WEDGED: step timed out after ${t}s: $*"
  return "$rc"
}
valid() { [ -f "$POISON" ] && echo "INVALID: compositor context poisoned at $(cut -d' ' -f1 "$POISON")"; }
step() { # <timeout-secs> <cmd...>: run a guest step; a timeout is logged as WEDGED
  local t=$1; shift
  timeout "$t" "$@"; local rc=$?
  [ "$rc" = 124 ] && log "WEDGED: step timed out after ${t}s: $*"
  return "$rc"
}
step() { # <timeout-secs> <cmd...>: run a guest step; a timeout is logged as WEDGED
  local t=$1; shift
  timeout "$t" "$@"; local rc=$?
  [ "$rc" = 124 ] && log "WEDGED: step timed out after ${t}s: $*"
  return "$rc"
}

# Prove the arm from the worker's own environment (ps -E shows it for our own processes).
prove_arm() { # <clone>
  local w
  for w in $(pgrep -f "[l]imina-vmm.*$(basename "$1")"); do
    ps -E -ww -o command= -p "$w" | tr ' ' '\n' | grep -c '^LIMINA_KK_NOLISTRESTART=0$' |
      sed "s/^/worker $w LIMINA_KK_NOLISTRESTART=0 count: /"
  done > "$EV/arm-env.txt"
  log "arm $ARM: $(tr '\n' ' ' < "$EV/arm-env.txt")"
}

log "enhanced boot"
boot "$ENH" "$IMG" "$ROOT/perf/listrestart-2026-10-07/work.noindex/aq-capture.png" ||
  { log "ABORT: enhanced boot"; kill_by_disk "$ENH"; keep_log "$ENH" enh; exit 3; }
watch_start "$ENH"
prove_arm "$ENH"
settle enh
for r in r1 r2; do
  LIMINA_SSH_PORT="$PORT" LIMINA_CAPTURE="$ROOT/perf/listrestart-2026-10-07/work.noindex/aq-capture.png" AQ_OUT="$EV/aquarium-$r" \
    step 900 scripts/perf/aquarium-run.sh vrend 25000 30000 > "$EV/aquarium-$r.txt" 2>&1
  log "aquarium $r exit=$? $(valid)"
done
"${SSH[@]}" "export XDG_RUNTIME_DIR=/run/user/1000; systemctl --user stop ff-bench 2>/dev/null || true" || true
watch_stop; shut "$ENH"; keep_log "$ENH" enh
log "point $LABEL done; read $EV/aquarium-r*/*-fps.png"
