#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Does upstream KosmicKrisp's never-reset command allocator ratchet host memory?
#
# Upstream KK (mesa 0126f4388a5) gives each VkCommandPool a free list of MTL4 command allocators,
# returns an allocator to it when recording ends, and begins the next command buffer on it with no
# reset in between: nothing under src/kosmickrisp/vulkan calls mtl_command_allocator_reset. Apple's
# allocator keeps its encoding heaps until it is reset, so the question is whether those heaps grow
# with every command buffer recorded, or plateau.
#
# Two oracles, both needed:
#   - attribution: LIMINA_KK_ALLOC_STATS=<n> on a KK built with the stats sampled at command-buffer
#     begin (the stock instrument samples only at reset, so on upstream it never fires). Every n
#     begins it dumps the live allocators' summed and largest -[MTL4CommandAllocator allocatedSize].
#   - impact: the worker's phys_footprint (spikes/hv-ledger-gap/ledger-dump, which does not suspend
#     the process), and IOAccelerator (graphics) from vmmap --summary at the closed states only.
#     vmmap suspends the worker, so it runs only between phases, never under load.
#
# Phases: a steady 30k-fish aquarium for STEADY_MIN minutes (does one long-lived workload grow?),
# then CYCLES open/close cycles of a 5k aquarium (does each launch leave something behind?).
# Compare closed against closed, never open against closed (spikes/vrend-region-leak/README.md).
#
# Usage: ratchet.sh <out-dir> <kk-icd.json>
#   STEADY_MIN=40 CYCLES=8 STATS_EVERY=2000 SRC_DISK=Fedora-Workstation-44.enhanced.raw
set -uo pipefail
OUT=${1:?out dir}; ICD=${2:?kk icd json}
STEADY_MIN=${STEADY_MIN:-40}; CYCLES=${CYCLES:-8}; STATS_EVERY=${STATS_EVERY:-2000}
SRC_DISK=${SRC_DISK:-Fedora-Workstation-44.enhanced.raw}
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel); cd "$ROOT"
mkdir -p "$OUT"
DISK="$OUT/ratchet.raw"
WLOG=/tmp/limina-worker-ratchet.log
DUMP=spikes/hv-ledger-gap/ledger-dump
SSHO=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR)
BUS='export XDG_RUNTIME_DIR=/run/user/1000 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus'
log() { echo "[$(date +%H:%M:%S)] $*" | tee -a "$OUT/run.log"; }
# Host-wide compressor and swap: what a VM lifecycle leaves behind after its worker exits (the
# graveyard in spikes/hv-ledger-gap/RESULTS.md). System-wide, so only meaningful on a quiet host.
host_mem() { # <label>
  local c
  c=$(vm_stat | awk '/stored in compressor/{gsub("\\.","",$5); s=$5}
                     /occupied by compressor/{gsub("\\.","",$5); o=$5}
                     END{printf "stored_pages=%s occupied_pages=%s", s, o}')
  echo "$1 $(date +%H:%M:%S) $c $(sysctl -n vm.swapusage)" >> "$OUT/host-mem.txt"
}

kill_by_disk() {
  for p in $(pgrep -f "[l]imina.*--disk $DISK"); do
    log "killing our own VM: $(ps -o pid,lstart,command -p "$p" | tail -n 1)"
    kill "$p"
  done
  for _ in $(seq 1 20); do pgrep -f "[l]imina.*--disk $DISK" >/dev/null || return 0; sleep 1; done
  for p in $(pgrep -f "[l]imina.*--disk $DISK"); do kill -9 "$p"; done
}

worker_pid() { # the largest-RSS limina-vmm on our disk; several processes match the pattern
  local pids; pids=$(pgrep -f "[l]imina-vmm.*$DISK" | tr '\n' ',' | sed 's/,$//')
  [ -n "$pids" ] && ps -o pid=,rss= -p "$pids" | sort -k2 -rn | awk 'NR==1{print $1}'
}

stats_line() { tail -n 400 "$WLOG" | grep 'LIMINA-ALLOC-STATS' | tail -n 1; }

# One CSV row: time, phase, footprint GiB, and the stats fields that answer the question.
sample() { # <phase>
  local pid pf s
  pid=$(worker_pid); [ -n "$pid" ] || return 1
  pf=$("$DUMP" "$pid" -a 2>/dev/null | awk '/^phys_footprint /{print $4}')
  s=$(stats_line)
  printf '%s,%s,%s,%s,%s,%s,%s,%s\n' "$(date +%s)" "$1" "${pf:-}" \
    "$(sed -n 's/.*begins=\([0-9]*\).*/\1/p' <<<"$s")" \
    "$(sed -n 's/.* live=\([0-9]*\).*/\1/p' <<<"$s")" \
    "$(sed -n 's/.* forgotten=\([0-9]*\).*/\1/p' <<<"$s")" \
    "$(sed -n 's/.* sum=\([0-9.]*\)MiB.*/\1/p' <<<"$s")" \
    "$(sed -n 's/.* max=\([0-9.]*\)MiB.*/\1/p' <<<"$s")" >> "$OUT/samples.csv"
}

snap() { # <label>: vmmap at a closed state only -- it suspends the worker
  local pid; pid=$(worker_pid)
  { echo "--- $1 $(date +%H:%M:%S)"; stats_line
    vmmap --summary "$pid" 2>/dev/null | grep -E "^IOAccelerator|Physical footprint:"; } >> "$OUT/snaps.txt"
  log "snap $1: $(grep -A3 -- "--- $1 " "$OUT/snaps.txt" | grep -E '^IOAccelerator \(graphics\)' | tr -s ' ')"
}

aquarium() { # <fish>
  "${SSH[@]}" "$BUS; systemctl --user stop ff-bench 2>/dev/null; systemctl --user reset-failed ff-bench 2>/dev/null; sleep 3
    systemd-run --user --unit=ff-bench --setenv=WAYLAND_DISPLAY=wayland-0 --setenv=MOZ_ENABLE_WAYLAND=1 \
      --setenv=MOZ_DISABLE_GPU_SANDBOX=1 --setenv=XDG_RUNTIME_DIR=/run/user/1000 \
      /usr/bin/firefox --kiosk 'https://webglsamples.org/aquarium/aquarium.html?numFish=$1'" >/dev/null 2>&1
}
close_ff() { "${SSH[@]}" "$BUS; systemctl --user stop ff-bench 2>/dev/null; true" >/dev/null 2>&1; }

kill_by_disk; rm -f "$OUT/ratchet.raw"
cp -c "$SRC_DISK" "$DISK" || { log "ABORT: clone failed"; exit 1; }
echo "ts,phase,footprint_gib,begins,live,forgotten,sum_mib,max_mib" > "$OUT/samples.csv"
: > "$OUT/host-mem.txt"; host_mem before-boot
: > "$OUT/snaps.txt"
{ echo "kk icd: $ICD"; ls -l "$(dirname "$ICD")"/libvulkan_kosmickrisp.dylib
  echo "limina $(git rev-parse --short HEAD) virglrs $(git -C third_party/virglrs rev-parse --short HEAD)" \
       "libkrun $(git -C third_party/libkrun rev-parse --short HEAD) disk $SRC_DISK"; } > "$OUT/provenance.txt"

log "boot (stats every $STATS_EVERY begins)"
LIMINA_KK_ICD="$ICD" LIMINA_KK_ALLOC_STATS="$STATS_EVERY" LIMINA_CPUS=4 LIMINA_RAM_MIB=4096 LIMINA_NET=1 \
  LIMINA_EXTRA_ARGS="--display-resolution 1280x800" LIMINA_WINDOW_CAPTURE="$OUT/capture.png" \
  RUST_LOG=warn,limina=info,krun_vmm=info,krun_devices=info LIMINA_POINTER_WIRE_TRACE=1 \
  LIMINA_DISK="$DISK" spikes/venus-draw-probe/boot-enhanced-efi-kk.sh > "$OUT/boot.txt" 2>&1 &
BPID=$!
PORT=$(scripts/wait-guest-ssh.sh "$WLOG" 400 "$BPID") || { log "ABORT: no ssh"; kill_by_disk; exit 1; }
SSH=(ssh -p "$PORT" "${SSHO[@]}" claude@127.0.0.1)
log "ssh on $PORT, worker $(worker_pid); settling 90 s"
sleep 90
sample idle; snap C0-idle

log "phase A: steady 30k aquarium for $STEADY_MIN min"
aquarium 30000
end=$(( $(date +%s) + STEADY_MIN * 60 ))
while [ "$(date +%s)" -lt "$end" ]; do sleep 30; sample steady; done
cp "$OUT/capture.png" "$OUT/steady-end.png" 2>/dev/null
close_ff; sleep 30; sample closed; snap A-closed

log "phase B: $CYCLES open/close cycles of a 5k aquarium"
for i in $(seq 1 "$CYCLES"); do
  aquarium 5000; sleep 60; sample "open$i"
  close_ff; sleep 30; sample "closed$i"
  case $i in 1|4|"$CYCLES") snap "B$i-closed" ;; esac
done

grep -c 'LIMINA-ALLOC-STATS' "$WLOG" > "$OUT/stats-count.txt"
grep 'LIMINA-ALLOC-STATS' "$WLOG" > "$OUT/stats.txt"
grep -iE 'poisoned|device is lost|LIMINA-KK\]' "$WLOG" > "$OUT/alarms.txt"
log "shutting down"
"${SSH[@]}" "sudo systemctl poweroff -i" >/dev/null 2>&1
for _ in $(seq 1 60); do pgrep -f "[l]imina.*--disk $DISK" >/dev/null || break; sleep 1; done
kill_by_disk
cp "$WLOG" "$OUT/worker.log"
host_mem after-exit; sleep 60; host_mem after-exit+60s
rm -f "$OUT/ratchet.raw"
log "done"
