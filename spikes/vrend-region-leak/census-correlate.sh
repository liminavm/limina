#!/bin/bash
# Correlate the GPU-memory ledger + Metal refcount census against the process footprint across
# workload open/close cycles.
#
# THE QUESTION THIS ANSWERS. vkr_budget.h states the discriminator: "if this ledger stays flat
# while the process footprint climbs, the leak is in OUR release path, not the guest's." The
# vrend region ratchet reached 17.3 GB against an 8192 MiB cap without the cap ever firing, so
# either the ledger is blind to the leaking allocations (they never pass a charged call site) or
# one of the Metal refcount deltas is growing and names the object class outright.
#
# Boot with the census on and the knob forwarded:
#   LIMINA_GPU_MEM_BUDGET_CENSUS=10 spikes/venus-draw-probe/boot-enhanced-efi-kk.sh
set -u
SSH="ssh -p ${LIMINA_SSH_PORT:-2222} -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR claude@127.0.0.1"
LOG=${LIMINA_WORKER_LOG:-/tmp/enhanced-efi-kk-worker.log}
# Pick the LARGEST-RSS match, not the first: more than one process matches this pattern (the
# supervisor spawns the worker with the same argv prefix), and `head -1` picked a tiny one,
# which reports a 1.6 MB footprint and no IOAccelerator lines at all — a snapshot that looks
# like "nothing allocated" rather than like a mismeasurement.
PID=$(ps -o pid=,rss= -p "$(pgrep -f 'limina-vmm --cpus' | tr '\n' ',' | sed 's/,$//')" \
      | sort -k2 -rn | head -1 | awk '{print $1}')
CYCLES=${CYCLES:-3}

snap() {
  echo "=== $1 ==="
  vmmap --summary "$PID" 2>/dev/null \
    | grep -E "^IOAccelerator \(graphics\)|^IOSurface|Physical footprint:" \
    | sed 's/^/  vm    /'
  # The most recent census tick: ledger live total, the Metal alloc-vs-free deltas, and the
  # per-context breakdown. NOTE the match strings are deliberately anchored on the em-dash
  # forms ("census —", "refs — ") — an earlier version filtered on "cap " and silently ate
  # every census line, because the live line itself ends in "of 8.0 GiB cap (0%)".
  grep -o "census — .*" "$LOG" | tail -1 | sed 's/^/  ledg  /'
  grep -o "refs — .*" "$LOG" | tail -1 | sed 's/^/  mtl   /' | cut -c1-200
  grep -o "ctx [0-9]* \[.*" "$LOG" | tail -2 | sed 's/^/  bkt   /' | cut -c1-200
  # KK-side BO census (LIMINA_KK_BOCENSUS=<secs>): MTLHeap+MTLBuffer pairs, the allocations
  # virglrenderer never sees. Note it does NOT cover MTLTextures, which are minted on a
  # different path (kk_image_plane_create_texture) — if regions climb while THIS stays flat,
  # textures are the next place to instrument.
  grep -o "bo alloc=.*" "$LOG" | tail -1 | sed 's/^/  kkbo  /' | cut -c1-150
  # Per-Objective-C-class live histogram, top 5. This is the one that names a leak outright:
  # a class whose live count tracks the region growth IS the leaked object.
  grep -A 6 "live objects by class" "$LOG" | tail -5 \
    | sed 's/.*LIMINA-KK-MTLCLASS\]   /  cls   /'
}

launch() {
  $SSH "export XDG_RUNTIME_DIR=/run/user/1000 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus
        systemctl --user stop ff-bench 2>/dev/null; sleep 3
        systemd-run --user --unit=ff-bench --setenv=WAYLAND_DISPLAY=wayland-0 \
          --setenv=MOZ_ENABLE_WAYLAND=1 --setenv=MOZ_DISABLE_GPU_SANDBOX=1 \
          --setenv=XDG_RUNTIME_DIR=/run/user/1000 ${AQ_EXTRA_ENV:-} \
          /usr/bin/firefox --kiosk 'https://webglsamples.org/aquarium/aquarium.html?numFish=5000'" >/dev/null 2>&1
  sleep 40
}
stopff() {
  $SSH "export XDG_RUNTIME_DIR=/run/user/1000 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus
        systemctl --user stop ff-bench 2>/dev/null; true" >/dev/null 2>&1
  sleep 30
}

snap "cycle 0 closed (baseline)"
for i in $(seq 1 "$CYCLES"); do
  launch; snap "cycle $i open"
  stopff; snap "cycle $i closed"
done
