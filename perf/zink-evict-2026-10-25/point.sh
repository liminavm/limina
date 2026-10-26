#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# One leg of the 2026-10-25 zink buffer-view eviction pass: GL before and after zink bounds the
# VkBufferViews each buffer caches (Mesa "zink: bound the buffer views a resource object caches").
# The change is in two zinks, so each arm moves both together:
#   old  host zink-on-KK at limina-kk 3d7a8fe4244 (built into /Volumes/mesa-cs/zink-kk-prefix-pre-evict)
#        and enhanced.test as it was before payload r43 (.bak-pre-r43: guest drivers 1b9049a75d7)
#   new  host zink at c40298fd0cc (/Volumes/mesa-cs/zink-kk-prefix) and enhanced.test now
#        (r43: guest drivers a2d20db7dab)
# limina, libkrun and virglrs are this tree's in both arms. Two tiers, both on enhanced.test with
# Firefox on hardware WebRender, as the dogfood runs it:
#   metal  the worker with LIMINA_GPU_METAL=1: GL is the guest's vendor zink on KosmicKrisp
#   vrend  no transport: GL is limina's virgl, rendered on the host by vrend on host zink
# Each point boots once and runs aquarium 20k/25k/30k x2 and perf-ledger x1 (glmark2, the GL
# replays and the llvmpipe control). Rows go to this directory's ledger, never perf/ledger.csv.
# Aquarium fps is only drawn on screen: read the *-fps.png crops afterwards.
#
# Usage: perf/zink-evict-2026-10-25/point.sh <label> <old|new> <metal|vrend>
set -uo pipefail
LABEL="${1:?label}"; ARM="${2:?old or new}"; TIER="${3:?metal or vrend}"
CACHE="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)/perf/zink-evict-2026-10-25/work.noindex/shader-cache-$ARM-$TIER"
HOST=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
# The images live in the main checkout, which a worktree reaches through its common git dir.
IMAGES=$(cd "$(git -C "$HOST" rev-parse --path-format=absolute --git-common-dir)/.." && pwd)
case "$ARM" in
  old) PREFIX=/Volumes/mesa-cs/zink-kk-prefix-pre-evict; IMG=$IMAGES/Fedora-Workstation-44.enhanced.test.bak-pre-r43.raw ;;
  new) PREFIX=/Volumes/mesa-cs/zink-kk-prefix; IMG=$IMAGES/Fedora-Workstation-44.enhanced.test.raw ;;
  *) echo "arm must be old or new" >&2; exit 1 ;;
esac
case "$TIER" in
  metal) WORKER=(LIMINA_GPU_METAL=1) ;;
  vrend) WORKER=() ;;
  *) echo "tier must be metal or vrend" >&2; exit 1 ;;
esac
WIRE=5
GENV=""; AQENV=""
ICD=/Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.aarch64.json
# LIMINA_HOST_GALLIUM puts the arm's prefix first on DYLD_LIBRARY_PATH (boot-enhanced-efi-kk.sh), so
# the libgallium the worker loads is that prefix's; both arms take the same path.
ARM_ENV=(LIMINA_KK_ICD="$ICD" MESA_SHADER_CACHE_DIR="$CACHE" MESA_PREFIX="$PREFIX" LIMINA_HOST_GALLIUM=zink ${WORKER[@]+"${WORKER[@]}"})
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel); cd "$ROOT"
DIR=perf/zink-evict-2026-10-25
EV="$DIR/evidence/$LABEL"; LEDGER="$DIR/ledger.csv"
mkdir -p "$EV" perf/zink-evict-2026-10-25/work.noindex/harness
[ -f "$LEDGER" ] || echo "date,commit,workload,metric,value,notes" > "$LEDGER"
log() { echo "[$(date +%H:%M:%S)] $*"; }
# The two disk clones, fixed names so nothing below deletes a computed path.
# The clone is absolute for the boot vehicle; the delete is relative to the root this script is in.
ENH=$ROOT/perf/zink-evict-2026-10-25/work.noindex/transport.raw
drop_clones() { rm -f perf/zink-evict-2026-10-25/work.noindex/transport.raw; }

V=$(git -C "$HOST/third_party/virglrs" rev-parse --short HEAD); L=$(git -C "$HOST/third_party/libkrun" rev-parse --short HEAD)
OS=$(sw_vers -productVersion)
H=$(git -C "$HOST" rev-parse --short HEAD)
# No commas: perf-ledger.sh writes its notes column unquoted.
NOTE="zink-evict $LABEL: arm $ARM tier $TIER ($(basename "$IMG")); host zink $(basename "$PREFIX"); virglrs $V + libkrun $L; limina $H; macOS $OS"
{ echo "$NOTE"; ls -l "$PREFIX"/lib/libgallium*.dylib /Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/libvulkan_kosmickrisp.dylib;
  ls -l "$HOST/target/debug/limina" "$HOST/target/debug/limina-vmm" "$HOST"/target/*/limina-gpu-host
  git -C /Volumes/mesa-cs/mesa status --short src/kosmickrisp; } > "$EV/provenance.txt" 2>&1
log "$NOTE"

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
    RUST_LOG=warn,limina=info,krun::vmm=info,krun_vmm=info,krun_devices=info ${3:+LIMINA_WINDOW_CAPTURE="$3"} \
    LIMINA_DISK="$1" "$HOST/spikes/venus-draw-probe/boot-enhanced-efi-kk.sh" > "$EV/boot-$tag.txt" 2>&1 &
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
    uname -r; rpm -q mesa-dri-drivers mesa-vulkan-drivers' > "$EV/settle-$1.txt" 2>&1
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
boot "$ENH" "$IMG" "$ROOT/perf/zink-evict-2026-10-25/work.noindex/aq-capture.png" ||
  { log "ABORT: enhanced boot"; kill_by_disk "$ENH"; keep_log "$ENH" enh; exit 3; }
watch_start "$ENH"
settle enh
# Prove the arm in the guest: the vendor drivers' revision, and what GL renders on.
step 120 "${SSH[@]}" "export XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0 $GENV
  cat /opt/limina/$WIRE/MESA_REV
  vulkaninfo --summary 2>&1 | grep -E 'deviceName|driverName'
  glmark2-es2-wayland -b build:duration=0.5 --size 128x128 2>&1 | grep -E 'GL_RENDERER|GL_VERSION'
  sudo dmesg | grep -m1 'limina extension'" > "$EV/arm.txt" 2>&1
log "arm $ARM tier $TIER: $(tr '\n' ' ' < "$EV/arm.txt")"
# And on the host: the libgallium the worker mapped must be the arm's (a path DYLD did not take
# would leave both arms on one zink and make the pass a confident null).
for p in $(pgrep -f "[l]imina-vmm.*$ENH"); do lsof -p "$p" 2>/dev/null | grep -o '/[^ ]*libgallium[^ ]*'; done | sort -u > "$EV/host-gallium.txt"
log "host libgallium: $(tr '\n' ' ' < "$EV/host-gallium.txt")"
if ! grep -q "^$PREFIX/lib/libgallium" "$EV/host-gallium.txt" || grep -vq "^$PREFIX/lib/libgallium" "$EV/host-gallium.txt"; then
  log "ABORT: the worker did not map $PREFIX's libgallium (see $EV/host-gallium.txt)"
  watch_stop; shut "$ENH"; keep_log "$ENH" enh; exit 4
fi
case "$ARM" in old) REV=1b9049a75d7 ;; new) REV=a2d20db7dab ;; esac
# A guest on the wrong drivers or the wrong tier measures something else: refuse the point.
if ! grep -q "^$REV" "$EV/arm.txt"; then
  log "ABORT: the guest's vendor drivers are not $REV (see $EV/arm.txt)"
  watch_stop; shut "$ENH"; keep_log "$ENH" enh; exit 4
fi
case "$TIER" in
  metal) grep -q 'GL_RENDERER.*zink.*\(KosmicKrisp\|Apple\)' "$EV/arm.txt" ;;
  vrend) grep -q 'GL_RENDERER.*virgl' "$EV/arm.txt" ;;
esac || { log "ABORT: GL is not on the $TIER tier (see $EV/arm.txt)"; watch_stop; shut "$ENH"; keep_log "$ENH" enh; exit 4; }
# Firefox keeps hardware WebRender: the texel-buffer churn the change bounds is WebRender's.
for r in r1 r2; do
  LIMINA_SSH_PORT="$PORT" LIMINA_CAPTURE="$ROOT/perf/zink-evict-2026-10-25/work.noindex/aq-capture.png" AQ_OUT="$EV/aquarium-$r" \
    AQ_EXTRA_ENV="$AQENV" \
    step 1200 scripts/perf/aquarium-run.sh "$TIER" 20000 25000 30000 > "$EV/aquarium-$r.txt" 2>&1
  log "aquarium $r exit=$? $(valid)"
done
case "$TIER" in metal) T=metal ;; vrend) T=venus ;; esac
LIMINA_SSH_PORT="$PORT" LIMINA_PERF_LEDGER="$LEDGER" LIMINA_PERF_RATE=60 LIMINA_PERF_TRANSPORT=$T step 900 scripts/perf-ledger.sh "$NOTE; run 1/1 $(valid)" \
  > "$EV/ledger-run1.txt" 2>&1
log "perf-ledger exit=$? $(valid)"; grep -E 'ABORT|GUARD|RENDERER|^[0-9-]+,.*(gl-replay|vk-replay|glmark2)' "$EV/ledger-run1.txt" | cut -d, -f3,5
"${SSH[@]}" "export XDG_RUNTIME_DIR=/run/user/1000; systemctl --user stop ff-bench 2>/dev/null || true" || true
watch_stop; shut "$ENH"; keep_log "$ENH" enh
log "point $LABEL done; read $EV/aquarium-r*/*-fps.png"
