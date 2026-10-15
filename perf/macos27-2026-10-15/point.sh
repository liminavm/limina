#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# One leg of the 2026-10-15 macOS 27 pass: the 2026-10-13 GPU-transport A/B
# (perf/metal-transport-2026-10-13/point.sh) repeated on macOS 27 with the transport as it now
# ships. Build limina's HEAD as pinned, boot metal.test, and run aquarium 25k/30k x2, perf-ledger
# x1 and vkmark x3 on one GPU transport; rows go to this directory's ledger, never to
# perf/ledger.csv. Aquarium fps is only drawn on screen: read the *-fps.png crops afterwards.
#
# READING THIS PASS: both arms boot the same image (metal.test: the stock F44 kernel at 4 KiB
# pages, Fedora's Mesa, limina's vendor GPU drivers delivered as a guest gets them) on the same
# host stack:
#   venus  the worker runs without the transport, so the guest is stock: GL through Fedora's zink
#          on the venus ICD, Vulkan on venus, the desktop on vrend -- as on 2026-10-13
#   metal  the worker runs with LIMINA_GPU_METAL=1: GL and Vulkan through the vendor drivers with
#          no environment, as a guest runs them, and the desktop itself on the transport
#   enhanced  the metal arm on enhanced.test instead (the 16 KiB limina kernel, limina's guest
#          Mesa, the vendor drivers), as the dogfood runs it -- Firefox included: hardware
#          WebRender, where the other two arms force software WebRender to match 2026-10-13
# The 2026-10-13 metal arm instead selected a hand-placed KosmicKrisp per client and left the
# desktop on vrend, so the metal rows of the two passes differ in more than the OS.
#
# Usage: perf/macos27-2026-10-15/point.sh <label> <venus|metal>
set -uo pipefail
LABEL="${1:?label}"; ARM="${2:?venus or metal}"
CACHE="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)/perf/macos27-2026-10-15/work.noindex/shader-cache-$ARM"
IMG=Fedora-Workstation-44.metal.test.raw
case "$ARM" in
  enhanced) WORKER=(LIMINA_GPU_METAL=1); GICD=/etc/vulkan/icd.d/limina-4.json
    IMG=Fedora-Workstation-44.enhanced.test.raw; GENV=""; AQENV="" ;;
  venus) WORKER=(); GICD=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json
    # GL through zink on the venus ICD.
    GENV="MESA_LOADER_DRIVER_OVERRIDE=zink GALLIUM_DRIVER=zink VK_DRIVER_FILES=$GICD"
    AQENV="--setenv=MESA_LOADER_DRIVER_OVERRIDE=zink --setenv=GALLIUM_DRIVER=zink --setenv=VK_DRIVER_FILES=$GICD" ;;
  metal) WORKER=(LIMINA_GPU_METAL=1); GICD=/etc/vulkan/icd.d/limina-4.json
    # The vendor drivers need nothing from the environment; vkmark still names the ICD.
    GENV=""; AQENV="" ;;
  *) echo "arm must be venus, metal or enhanced" >&2; exit 1 ;;
esac
ICD=/Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.aarch64.json
ARM_ENV=(LIMINA_KK_ICD="$ICD" MESA_SHADER_CACHE_DIR="$CACHE" ${WORKER[@]+"${WORKER[@]}"})
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel); cd "$ROOT"
DIR=perf/macos27-2026-10-15
EV="$DIR/evidence/$LABEL"; LEDGER="$DIR/ledger.csv"
mkdir -p "$EV" perf/macos27-2026-10-15/work.noindex/harness
[ -f "$LEDGER" ] || echo "date,commit,workload,metric,value,notes" > "$LEDGER"
log() { echo "[$(date +%H:%M:%S)] $*"; }
# The two disk clones, fixed names so nothing below deletes a computed path.
ENH=perf/macos27-2026-10-15/work.noindex/transport.raw
drop_clones() { rm -f perf/macos27-2026-10-15/work.noindex/transport.raw; }

V=$(git -C third_party/virglrs rev-parse --short HEAD); L=$(git -C third_party/libkrun rev-parse --short HEAD)
OS=$(sw_vers -productVersion)
M=$(git -C /Volumes/mesa-cs/mesa rev-parse --short HEAD)
# No commas: perf-ledger.sh writes its notes column unquoted.
NOTE="macos27 $LABEL: arm $ARM ($IMG); host mesa $M; virglrs $V + libkrun $L; limina $(git rev-parse --short HEAD); macOS $OS"
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
    RUST_LOG=warn,limina=info,krun::vmm=info,krun_vmm=info,krun_devices=info ${3:+LIMINA_WINDOW_CAPTURE="$3"} \
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
boot "$ENH" "$IMG" "$ROOT/perf/macos27-2026-10-15/work.noindex/aq-capture.png" ||
  { log "ABORT: enhanced boot"; kill_by_disk "$ENH"; keep_log "$ENH" enh; exit 3; }
watch_start "$ENH"
settle enh
# Prove the arm in the guest: which device the arm's ICD gives, and what GL renders on.
step 120 "${SSH[@]}" "export XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0 $GENV
  vulkaninfo --summary 2>&1 | grep -E 'deviceName|driverName'
  glmark2-es2-wayland -b build:duration=0.5 --size 128x128 2>&1 | grep -E 'GL_RENDERER|GL_VERSION'
  sudo dmesg | grep -m1 'limina extension'" > "$EV/arm.txt" 2>&1
log "arm $ARM: $(tr '\n' ' ' < "$EV/arm.txt")"
# Aquarium first, then perf-ledger and vkmark. The watchdog marks every row after a poisoned
# compositor context INVALID.
#
# Firefox composites with software WebRender in both arms. Its GL compositor on zink segfaults
# on the metal arm (zink_set_sampler_views, even on about:blank), and on the venus arm a kiosk
# window is scanned out directly from a buffer the host reads back black, so with hardware
# WebRender neither arm yields a frame. WebGL still renders through zink on the arm's ICD; the
# page's pixels reach the compositor through shared memory. The pref lives in the profile, which
# Firefox creates on its first start, hence the throwaway launch.
[ "$ARM" = enhanced ] || step 120 "${SSH[@]}" 'export XDG_RUNTIME_DIR=/run/user/1000 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus
  systemd-run -q --user --unit=ff-profile --setenv=WAYLAND_DISPLAY=wayland-0 /usr/bin/firefox --headless about:blank
  for i in $(seq 1 30); do ls -d ~/.config/mozilla/firefox/*.default-release >/dev/null 2>&1 && break; sleep 1; done
  sleep 5; systemctl --user stop ff-profile; systemctl --user reset-failed ff-profile 2>/dev/null; sleep 2
  for d in ~/.config/mozilla/firefox/*.default-release; do
    echo "user_pref(\"gfx.webrender.software\", true);" > "$d/user.js"; echo "pref written: $d/user.js"
  done' > "$EV/ff-profile.txt" 2>&1
log "firefox profile: $(tr '\n' ' ' < "$EV/ff-profile.txt")"
LEDGER_RUNS="1"; [ "${AQ_ONLY:-0}" = 1 ] && LEDGER_RUNS=""
for r in r1 r2; do
  LIMINA_SSH_PORT="$PORT" LIMINA_CAPTURE="$ROOT/perf/macos27-2026-10-15/work.noindex/aq-capture.png" AQ_OUT="$EV/aquarium-$r" \
    AQ_EXTRA_ENV="$AQENV" \
    step 900 scripts/perf/aquarium-run.sh "$ARM" 25000 30000 > "$EV/aquarium-$r.txt" 2>&1
  log "aquarium $r exit=$? $(valid)"
done
for r in $LEDGER_RUNS; do
  LIMINA_SSH_PORT="$PORT" LIMINA_PERF_LEDGER="$LEDGER" LIMINA_PERF_RATE=60 LIMINA_PERF_TRANSPORT="${ARM/enhanced/metal}" step 900 scripts/perf-ledger.sh "$NOTE; run $r/1 $(valid)" \
    > "$EV/ledger-run$r.txt" 2>&1
  log "perf-ledger run $r exit=$? $(valid)"; grep -E 'ABORT|GUARD|RENDERER|^[0-9-]+,.*(gl-replay|vk-replay|glmark2)' "$EV/ledger-run$r.txt" | cut -d, -f3,5
done
VK_RUNS="1 2 3"; [ "${AQ_ONLY:-0}" = 1 ] && VK_RUNS=""
for r in $VK_RUNS; do
  step 600 "${SSH[@]}" "export XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0 VK_DRIVER_FILES=$GICD; vkmark -s 1280x720" \
    > "$EV/vkmark-run$r.txt" 2>&1
  s=$(sed -n 's/.*vkmark Score: \([0-9]*\).*/\1/p' "$EV/vkmark-run$r.txt")
  log "vkmark run $r: ${s:-NO SCORE} $(valid)"; [ -n "$s" ] && row "vkmark-default-$ARM" score "$s" "run $r/3 $(valid)"
done
"${SSH[@]}" "export XDG_RUNTIME_DIR=/run/user/1000; systemctl --user stop ff-bench 2>/dev/null || true" || true
watch_stop; shut "$ENH"; keep_log "$ENH" enh
log "point $LABEL done; read $EV/aquarium-r*/*-fps.png"
