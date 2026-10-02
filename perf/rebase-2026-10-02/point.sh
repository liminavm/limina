#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# One leg of the 2026-10-02 rebase pass: build limina's HEAD as pinned, boot an enhanced guest on
# today's host stack, and run aquarium 25k/30k x2, perf-ledger x1 and vkmark x3. A copy of
# perf/rebase-2026-10-02/point.sh; rows go to this directory's ledger, never to
# perf/ledger.csv. Aquarium fps is only drawn on screen: read the *-fps.png crops afterwards.
#
# READING THIS PASS: the host is the rebased stack in both arms (limina-kk, virglrs on the rebased
# C fork, libkrun). The arms differ only in the guest image:
#   new  Fedora-Workstation-44.enhanced.raw          payload r30 (mesa 26.2.3-2, kernel 7.1.13)
#   old  Fedora-Workstation-44.enhanced.bak-pre-r28.raw  payload r27 (mesa 26.1.8, kernel 7.1.8)
# so the guest half of the rebase is a bracketed A/B, and the host half is read against
# perf/rebase-2026-10-02/ledger.csv (same instruments, the pre-rebase host), with the
# gl-replay-llvmpipe control saying whether the two days' hosts are comparable.
#
# Usage: perf/rebase-2026-10-02/point.sh <label> <new|old>
set -uo pipefail
LABEL="${1:?label}"; ARM="${2:?new or old}"
CACHE="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)/perf/rebase-2026-10-02/work.noindex/shader-cache-$ARM"
case "$ARM" in
  new) IMG=Fedora-Workstation-44.enhanced.raw ;;
  old) IMG=Fedora-Workstation-44.enhanced.bak-pre-r28.raw ;;
  *) echo "arm must be new or old" >&2; exit 1 ;;
esac
ICD=/Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.aarch64.json
ARM_ENV=(LIMINA_KK_ICD="$ICD" MESA_SHADER_CACHE_DIR="$CACHE")
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel); cd "$ROOT"
DIR=perf/rebase-2026-10-02
EV="$DIR/evidence/$LABEL"; LEDGER="$DIR/ledger.csv"
mkdir -p "$EV" perf/rebase-2026-10-02/work.noindex/harness
[ -f "$LEDGER" ] || echo "date,commit,workload,metric,value,notes" > "$LEDGER"
log() { echo "[$(date +%H:%M:%S)] $*"; }
# The two disk clones, fixed names so nothing below deletes a computed path.
ENH=perf/rebase-2026-10-02/work.noindex/trend-enh.raw
drop_clones() { rm -f perf/rebase-2026-10-02/work.noindex/trend-enh.raw; }

V=$(git -C third_party/virglrs rev-parse --short HEAD); L=$(git -C third_party/libkrun rev-parse --short HEAD)
M=$(git -C /Volumes/mesa-cs/mesa rev-parse --short HEAD)
# No commas: perf-ledger.sh writes its notes column unquoted.
NOTE="rebase $LABEL: guest $ARM ($IMG); host mesa $M; virglrs $V + libkrun $L; limina $(git rev-parse --short HEAD)"
{ echo "$NOTE"; ls -l /Volumes/mesa-cs/zink-kk-prefix/lib/libgallium*.dylib /Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/libvulkan_kosmickrisp.dylib;
  git -C /Volumes/mesa-cs/mesa status --short src/kosmickrisp; } > "$EV/provenance.txt"
log "build $NOTE"
cargo xtask build > "$EV/build.log" 2>&1 || { log "ABORT: build failed, see $EV/build.log"; exit 2; }

# A VM whose boot failed is still running; take it down by its disk before bailing out.
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
# The boot vehicle truncates its per-disk worker log on the next boot, so keep it with the point.
keep_log() { # <clone> <tag>
  local wl; wl="/tmp/limina-worker-$(basename "$1" .raw).log"
  cp "$wl" "$EV/worker-$2.log" 2>/dev/null
  log "worker-$2: $(grep -c poisoned "$EV/worker-$2.log" 2>/dev/null || true) poisoned-context lines"
}
SSHO=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR)
boot() { # <clone> <source-image> [capture] -> sets PORT, BPID, SSH
  drop_clones; cp -c "$2" "$1" || return 1
  local tag; tag=$(basename "$1" .raw)
  env ${ARM_ENV[@]+"${ARM_ENV[@]}"} \
    LIMINA_CPUS=4 LIMINA_RAM_MIB=4096 LIMINA_NET=1 LIMINA_EXTRA_ARGS="--display-resolution 1280x800" \
    RUST_LOG=warn,limina=info,krun_vmm=info,krun_devices=info ${3:+LIMINA_WINDOW_CAPTURE="$3"} \
    LIMINA_DISK="$1" spikes/venus-draw-probe/boot-enhanced-efi-kk.sh > "$EV/boot-$tag.txt" 2>&1 &
  BPID=$!
  PORT=$(scripts/wait-guest-ssh.sh "/tmp/limina-worker-$tag.log" 400 "$BPID") || return 1
  SSH=(ssh -p "$PORT" "${SSHO[@]}" claude@127.0.0.1)
  pin_display "$tag"
}
# Pin the guest display before measuring. --display-resolution fixes the size, but the EDID carries
# the host screen's dpi and refresh, so on the built-in Retina GNOME picks scale 1.333 at 120 Hz;
# the earlier passes ran at 1.0 and 60 Hz. monitors.xml applies at session start with no dialog, so write
# it, reboot (the supervisor relaunches the worker on the same forward), and verify.
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
watch_stop() { kill "$WATCH" 2>/dev/null; wait "$WATCH" 2>/dev/null; }
valid() { [ -f "$POISON" ] && echo "INVALID: compositor context poisoned at $(cut -d' ' -f1 "$POISON")"; }
step() { # <timeout-secs> <cmd...>: run a guest step; a timeout is logged as WEDGED
  local t=$1; shift
  timeout "$t" "$@"; local rc=$?
  [ "$rc" = 124 ] && log "WEDGED: step timed out after ${t}s: $*"
  return "$rc"
}

# --- enhanced guest
log "enhanced boot"
boot "$ENH" "$IMG" "$ROOT/perf/rebase-2026-10-02/work.noindex/aq-capture.png" ||
  { log "ABORT: enhanced boot"; kill_by_disk "$ENH"; keep_log "$ENH" enh; exit 3; }
watch_start "$ENH"
settle enh
# Aquarium first: it is the vrend reading, and the venus clients (perf-ledger, vkmark) run after it
# so a poisoned compositor context cannot cost the vrend number.
LEDGER_RUNS="1"
for r in r1 r2; do
  LIMINA_SSH_PORT="$PORT" LIMINA_CAPTURE="$ROOT/perf/rebase-2026-10-02/work.noindex/aq-capture.png" AQ_OUT="$EV/aquarium-$r" \
    step 900 scripts/perf/aquarium-run.sh vrend 25000 30000 > "$EV/aquarium-$r.txt" 2>&1
  log "aquarium $r exit=$? $(valid)"
done
for r in $LEDGER_RUNS; do
  LIMINA_SSH_PORT="$PORT" LIMINA_PERF_LEDGER="$LEDGER" LIMINA_PERF_RATE=60 step 900 scripts/perf-ledger.sh "$NOTE; run $r/1 $(valid)" \
    > "$EV/ledger-run$r.txt" 2>&1
  log "perf-ledger run $r exit=$? $(valid)"; grep -E 'ABORT|GUARD|^[0-9-]+,.*(gl-replay|vk-replay|glmark2)' "$EV/ledger-run$r.txt" | cut -d, -f3,5
done
for r in 1 2 3; do
  step 600 "${SSH[@]}" 'export XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0; vkmark -s 1280x720' \
    > "$EV/vkmark-run$r.txt" 2>&1
  s=$(sed -n 's/.*vkmark Score: \([0-9]*\).*/\1/p' "$EV/vkmark-run$r.txt")
  log "vkmark run $r: ${s:-NO SCORE} $(valid)"; [ -n "$s" ] && row vkmark-default-venus score "$s" "run $r/3 $(valid)"
done
"${SSH[@]}" "export XDG_RUNTIME_DIR=/run/user/1000; systemctl --user stop ff-bench 2>/dev/null || true" || true
watch_stop; shut "$ENH"; keep_log "$ENH" enh
log "point $LABEL done; read $EV/aquarium-r*/*-fps.png"
