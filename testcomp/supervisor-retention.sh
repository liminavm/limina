#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# The supervisor-holder arm of spikes/venus-churn-retention/buffer-lifetime-matrix.md §1:
# **does host memory come back when the compositor quits?**
#
# M3b/M3c closed both vkr holder columns — a client-buffer lifetime bug cannot produce §1's
# residual, however deliberately the guest misbehaves. What is left is the holder no vkr sweep
# can reach: the SUPERVISOR, a different process, holding scanout IOSurfaces it was handed by
# Mach port. RESULTS.md §0.4 already proved it holds `SURFACE_STORE_CAP` x one framebuffer
# *during* a run (436.8 M -> 896 K when the supervisor alone is SIGKILLed). What was never
# measured is the part §1 actually describes: whether that residual ever goes away.
#
# The mechanism this arm exists to test. Eviction is ARRIVAL-DRIVEN — `SurfaceStore::insert`
# drops the oldest only when a new surface arrives — and publishes happen once per
# `IOSurfaceCreate`. So when the compositor quits, nothing new is ever published, nothing is
# ever evicted, and its last 32 (store) + 8 (frame cache) framebuffers are pinned for the life
# of the supervisor. Predicted residual at 1920x1080: 40 x 7.9 MiB = ~316 MiB, permanent.
#
# Oracle: `owned unmapped` BYTES from `vmmap -summary` of the WORKER — memory billed to a task
# but not mapped into it, which is where an IOSurface retained by another process lands. It is
# the worker's number even though the supervisor is what holds it, because an IOSurface's
# storage bills to the task that CREATED it. Bytes, never the region count: regions coalesce,
# and a count-based read moved by ONE across a 20x change in bytes (RESULTS.md §0.4).
#
# Read every number SETTLED — two consecutive equal samples — the discipline
# crates/limina-test/tests/venus_fd_census.rs uses. A single sample mid-teardown is how a
# reclaim that did happen gets recorded as one that did not.
#
#   ./supervisor-retention.sh              baseline -> churn -> quit -> settled residual
#   ./supervisor-retention.sh splitkill    the same, then SIGKILL the supervisor alone to
#                                          attribute the residual to it (the worker survives)
#
# Run from the repo root against a guest booted windowed — the holder lives in `window::run`,
# so a --display-capture boot exercises none of it:
#
#   LIMINA_DISK=<enhanced.raw> LIMINA_EXTRA_ARGS="--display-resolution 1920x1080" \
#     spikes/venus-draw-probe/boot-enhanced-efi-kk.sh

set -uo pipefail

PORT="${PORT:-2222}"
FRAMES="${FRAMES:-300}"
SSH=(ssh -p "$PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes claude@127.0.0.1)
ICD=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json

g() { "${SSH[@]}" "$@" 2>/dev/null; }

# TRAP: the SUPERVISOR's own argv contains `--vmm-bin target/debug/limina-vmm`, so a bare
# `pgrep -f target/debug/limina-vmm` matches the supervisor FIRST and every measurement below
# then reads the wrong process — which looks exactly like "no retention" (the supervisor has no
# `owned unmapped` row at all, because IOSurface storage bills to the worker that created it).
# Match on an argument only the real process has.
worker_pid() { pgrep -f "limina-vmm --cpus" | head -1; }
supervisor_pid() { pgrep -f "limina --vmm-bin" | head -1; }

# `owned unmapped` BYTES for a pid. Prints raw bytes, or 0 when the row is absent — which is
# what a healthy resting worker looks like, NOT a failed read (the row only exists once there
# is something in the bucket).
owned_unmapped() {
  local pid="$1"
  vmmap -summary "$pid" 2>/dev/null | awk '
    /owned unmapped/ && !/graphics/ {
      for (i = 1; i <= NF; i++) {
        if ($i ~ /^[0-9.]+[KMG]?$/) {
          n = $i + 0; u = substr($i, length($i))
          if (u == "K") n *= 1024
          else if (u == "M") n *= 1024 * 1024
          else if (u == "G") n *= 1024 * 1024 * 1024
          printf "%d\n", n
          exit
        }
      }
    }
    END { }
  ' | head -1
}

mib() { awk -v b="${1:-0}" 'BEGIN { printf "%.1f MiB", b / 1048576 }'; }

# Sample until two consecutive readings agree within 1 MiB, or we run out of patience. Prints
# the settled byte count; prints the last sample with a loud marker if it never settles, so an
# unsettled number can never be mistaken for a settled one.
settled() {
  local pid="$1" prev=-1 cur
  for _ in $(seq 1 12); do
    cur=$(owned_unmapped "$pid"); cur=${cur:-0}
    if [ "$prev" -ge 0 ] && [ "$((cur > prev ? cur - prev : prev - cur))" -lt 1048576 ]; then
      echo "$cur"; return 0
    fi
    prev=$cur
    sleep 3
  done
  echo "UNSETTLED:$cur"
  return 1
}

report() {
  local label="$1" pid="$2" v
  v=$(settled "$pid")
  case "$v" in
    UNSETTLED:*) echo "  $label: $(mib "${v#UNSETTLED:}") !! NEVER SETTLED — do not use this number";;
    *) echo "  $label: $(mib "$v")"; echo "$v" > /tmp/.last-settled;;
  esac
}

WPID=$(worker_pid)
SPID=$(supervisor_pid)
[ -n "$WPID" ] || { echo "!! no limina-vmm worker running — boot the guest first"; exit 1; }
[ -n "$SPID" ] || { echo "!! no limina supervisor running"; exit 1; }
echo "worker pid=$WPID  supervisor pid=$SPID  frames=$FRAMES"

# testcomp needs DRM master, which GNOME holds on a seated boot. This also removes the desktop
# as a second publisher, so every surface in the store came from the vehicle.
echo "== isolating to multi-user.target (dropping the seated session)"
g "sudo -n systemctl isolate multi-user.target" >/dev/null 2>&1
sleep 5

echo "== baseline"
report "worker owned unmapped" "$WPID"
BASE=$(cat /tmp/.last-settled)

echo "== churn $FRAMES fresh venus scanout buffers, then a CLEAN exit"
CHURN=$(g "cd /tmp && sudo -n env VK_DRIVER_FILES=$ICD RUST_LOG=info ./limina-testcomp churn $FRAMES 2>&1 | tail -3")
echo "$CHURN" | sed 's/^/  /'
# The vehicle-ran assert. A retention number means nothing without evidence the workload
# actually allocated — a fallback that never mints a host surface reads as a perfect pass.
echo "$CHURN" | grep -q "CHURN DONE" || { echo "!! churn did not complete — the numbers below mean nothing"; exit 1; }
# And the guest process is really gone, so nothing guest-side can still be holding.
g "pgrep -f limina-test[c]omp" >/dev/null 2>&1 && { echo "!! a testcomp process survived the run"; exit 1; }

echo "== after the compositor quit (settled)"
report "worker owned unmapped" "$WPID"
AFTER=$(cat /tmp/.last-settled)

echo
echo "  residual over baseline: $(mib $((AFTER - BASE)))"
echo "  (predicted if the supervisor pins its caps: 40 x one framebuffer)"

if [ "${1:-}" = splitkill ]; then
  echo
  echo "== SIGKILL the supervisor ALONE (the worker must survive to be measured)"
  # SIGKILL, not SIGTERM: a clean shutdown tears the worker down on the way out, and then
  # nothing can be attributed to either process (RESULTS.md §0.4 lost a run to exactly this).
  kill -9 "$SPID"
  sleep 3
  kill -0 "$WPID" 2>/dev/null || { echo "!! the worker died with the supervisor — nothing to attribute"; exit 1; }
  report "worker owned unmapped" "$WPID"
  FREED=$(cat /tmp/.last-settled)
  echo
  echo "  freed by the supervisor's death: $(mib $((AFTER - FREED)))"
fi
