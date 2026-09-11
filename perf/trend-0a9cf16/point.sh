#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# One point of the virglrs trend sweep: build limina's HEAD against a given virglrs + libkrun pair,
# then run the usual battery on it -- enhanced guest (aquarium 25k/30k x2, perf-ledger x3, vkmark
# x3) and stock guest (Basemark Graphics suite, second run scored). Rows go to this directory's
# ledger, never to perf/ledger.csv. Aquarium fps is only drawn on screen: read the *-fps.png crops
# afterwards and add those rows by hand.
#
# Usage: perf/trend-0a9cf16/point.sh <label> <virglrs-rev> <libkrun-rev>
# STOCK_ONLY=1 skips the enhanced guest and runs the Basemark arm alone.
# LIBKRUN_PATCH=<file> applies a patch to libkrun for the build only; it is reverted right after.
# Leaves third_party/virglrs and third_party/libkrun checked out (detached) at the given revs.
set -uo pipefail
LABEL="${1:?label}"; VREV="${2:?virglrs rev}"; LREV="${3:?libkrun rev}"
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel); cd "$ROOT"
DIR=perf/trend-0a9cf16
EV="$DIR/evidence/$LABEL"; LEDGER="$DIR/ledger.csv"
mkdir -p "$EV" perf-work.noindex/harness
[ -f "$LEDGER" ] || echo "date,commit,workload,metric,value,notes" > "$LEDGER"
log() { echo "[$(date +%H:%M:%S)] $*"; }
# The two disk clones, fixed names so nothing below deletes a computed path.
ENH=perf-work.noindex/trend-enh.raw
STOCK=perf-work.noindex/trend-stock.raw
drop_clones() { rm -f perf-work.noindex/trend-enh.raw perf-work.noindex/trend-stock.raw; }

git -C third_party/virglrs checkout -q --detach "$VREV" || { log "ABORT: virglrs checkout"; exit 1; }
git -C third_party/libkrun checkout -q --detach "$LREV" || { log "ABORT: libkrun checkout"; exit 1; }
if [ -n "${LIBKRUN_PATCH:-}" ]; then
  git -C third_party/libkrun apply "$ROOT/$LIBKRUN_PATCH" || { log "ABORT: libkrun patch"; exit 1; }
fi
V=$(git -C third_party/virglrs rev-parse --short HEAD); L=$(git -C third_party/libkrun rev-parse --short HEAD)${LIBKRUN_PATCH:+ patched with $(basename "$LIBKRUN_PATCH")}
M=$(git -C /Volumes/mesa-cs/mesa rev-parse --short HEAD)
# No commas: perf-ledger.sh writes its notes column unquoted.
NOTE="trend $LABEL: virglrs $V + libkrun $L; host mesa $M; limina $(git rev-parse --short HEAD)"
{ echo "$NOTE"; ls -l /Volumes/mesa-cs/zink-kk-prefix/lib/libgallium*.dylib /Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/libvulkan_kosmickrisp.dylib; } > "$EV/provenance.txt"
log "build $NOTE"
cargo xtask build > "$EV/build.log" 2>&1; brc=$?
[ -n "${LIBKRUN_PATCH:-}" ] && git -C third_party/libkrun apply -R "$ROOT/$LIBKRUN_PATCH"   # built; leave the tree clean
[ "$brc" = 0 ] || { log "ABORT: build failed, see $EV/build.log"; exit 2; }

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
  env LIMINA_CPUS=4 LIMINA_RAM_MIB=4096 LIMINA_NET=1 LIMINA_EXTRA_ARGS="--display-resolution 1280x800" \
    RUST_LOG=warn,limina=info,krun_vmm=info,krun_devices=info ${3:+LIMINA_WINDOW_CAPTURE="$3"} \
    LIMINA_DISK="$1" spikes/venus-draw-probe/boot-enhanced-efi-kk.sh > "$EV/boot-$tag.txt" 2>&1 &
  BPID=$!
  PORT=$(scripts/wait-guest-ssh.sh "/tmp/limina-worker-$tag.log" 400 "$BPID") || return 1
  SSH=(ssh -p "$PORT" "${SSHO[@]}" claude@127.0.0.1)
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

# WATCHDOG. Before virglrs 42008bb a venus client can poison the compositor's vrend context: the
# screen flickers, the shell chrome vanishes, and every number taken afterwards is not a
# measurement. A background watch on the worker log drops a POISONED marker the moment the first
# refusal lands; every row written after it says INVALID. Every guest step also runs under a
# timeout, so a wedged guest costs its point and never strands the sweep.
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
if [ "${STOCK_ONLY:-0}" != 1 ]; then
log "enhanced boot"
boot "$ENH" Fedora-Workstation-44.enhanced.raw "$ROOT/perf-work.noindex/aq-capture.png" ||
  { log "ABORT: enhanced boot"; kill_by_disk "$ENH"; keep_log "$ENH" enh; exit 3; }
watch_start "$ENH"
settle enh
# Aquarium first: it is the vrend reading, and before virglrs 42008bb the first venus client
# poisons the compositor's vrend context for the rest of the session. Every venus client
# (perf-ledger, vkmark) therefore runs after it.
for r in r1 r2; do
  LIMINA_SSH_PORT="$PORT" LIMINA_CAPTURE="$ROOT/perf-work.noindex/aq-capture.png" AQ_OUT="$EV/aquarium-$r" \
    step 900 scripts/perf/aquarium-run.sh vrend 25000 30000 > "$EV/aquarium-$r.txt" 2>&1
  log "aquarium $r exit=$? $(valid)"
done
for r in 1 2 3; do
  LIMINA_SSH_PORT="$PORT" LIMINA_PERF_LEDGER="$LEDGER" step 900 scripts/perf-ledger.sh "$NOTE; run $r/3 $(valid)" \
    > "$EV/ledger-run$r.txt" 2>&1
  log "perf-ledger run $r exit=$? $(valid)"; grep -E 'ABORT|GUARD|^[0-9-]+,.*(gl-replay|vk-replay|glmark2)' "$EV/ledger-run$r.txt" | cut -d, -f3,5
done
for r in 1 2 3; do
  step 600 "${SSH[@]}" 'export XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0; vkmark -s 1280x720' \
    > "$EV/vkmark-run$r.txt" 2>&1
  s=$(sed -n 's/.*vkmark Score: \([0-9]*\).*/\1/p' "$EV/vkmark-run$r.txt")
  log "vkmark run $r: ${s:-NO SCORE} $(valid)"; [ -n "$s" ] && row vkmark-default-venus score "$s" "run $r/3 $(valid)"
done
watch_stop; shut "$ENH"; keep_log "$ENH" enh
fi
POISON="$EV/POISONED-stock"

# --- stock guest
log "stock boot"
boot "$STOCK" Fedora-Workstation-44.stock.test.raw ||
  { log "ABORT: stock boot"; kill_by_disk "$STOCK"; keep_log "$STOCK" stock; exit 4; }
for f in client-basemark.sh marionette.py tap-keys.py; do   # one frozen harness for every point
  git -C third_party/virglrs show "d0416c9:harness/vm/$f" > "perf-work.noindex/harness/$f"
done
scp -P "$PORT" "${SSHO[@]}" -q perf-work.noindex/harness/client-basemark.sh perf-work.noindex/harness/marionette.py \
  perf-work.noindex/harness/tap-keys.py claude@127.0.0.1:/tmp/
"${SSH[@]}" 'chmod +x /tmp/client-basemark.sh; sudo systemctl isolate graphical.target'
watch_start "$STOCK"
settle stock
step 2400 "${SSH[@]}" '/tmp/client-basemark.sh' > "$EV/basemark.txt" 2>&1
log "basemark exit=$?"; grep -E 'renderer=|REFUS' "$EV/basemark.txt"
awk '/--- scores \(run 2\)/{on=1} on && /Test$/{t=$0; getline v; print t "=" v}' "$EV/basemark.txt" |
while IFS='=' read -r t v; do
  case "$t" in
    "WebGL 1.0.2 Test") w=basemark-webgl102-vrend ;; "WebGL 2.0 Test") w=basemark-webgl20-vrend ;;
    "Shader Pipeline Test") w=basemark-shader-pipeline-vrend ;; "Draw-call Stress Test") w=basemark-drawcall-stress-vrend ;;
    "Geometry Stress Test") w=basemark-geometry-stress-vrend ;; "Canvas Test") w=basemark-canvas-vrend ;;
    "SVG Test") w=basemark-svg-vrend ;; *) continue ;;
  esac
  log "basemark $t $v $(valid)"; row "$w" score "$v" "stock guest; second suite run scored $(valid)"
done
watch_stop; shut "$STOCK"; keep_log "$STOCK" stock
log "point $LABEL done; read $EV/aquarium-r*/*-fps.png"
