#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# M3b: drive the teardown paths of spikes/venus-churn-retention/buffer-lifetime-matrix.md §4
# and report what the host actually did, per path.
#
# This is a DISCRIMINATOR, not a confirmation. The matrix originally assumed a SIGKILLed client
# lands in vkr's bare-free() branch and leaks; reading vkr_context.c showed the opposite — the
# sweep is gated on ctx->instance being LIVE (vkr_context.c:1005), and a SIGKILLed client never
# calls vkDestroyInstance, so abrupt exit is the path that runs the FULL sweep. So the job here
# is to find out which path leaves a residual, not to prove a predicted one does.
#
# Run from the repo root, with a guest already booted by
#   LIMINA_DISK=<enhanced.raw> LIMINA_GLOBAL_SCANOUT=1 LIMINA_GPU_MEM_BUDGET_CENSUS=10 \
#     RUST_LOG="warn,krun_rutabaga_gfx::virgl_renderer=info" \
#     spikes/venus-draw-probe/boot-enhanced-efi-kk.sh
#
# The RUST_LOG matters: the import and context-destroy lines are vkr_log = VIRGL_LOG_LEVEL_INFO
# and are INVISIBLE at the default `warn`, while the budget lines (vkr_log_error) are not — so a
# log filtered to `warn` looks instrumented while telling you nothing about the import path.

set -uo pipefail

PORT="${PORT:-2222}"
WORKER_LOG="${WORKER_LOG:-/tmp/enhanced-efi-kk-worker.log}"
SIZE="${SIZE:-1920x1080}"   # big enough that retention clears the `owned unmapped` noise floor
SSH=(ssh -p "$PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes claude@127.0.0.1)
ICD=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json

g() { "${SSH[@]}" "$@" 2>/dev/null; }

worker_pid() { pgrep -f "target/debug/limina-vmm" | head -1; }

# `owned unmapped` BYTES, not regions: regions coalesce, so a region count understates retention
# (see crates/limina-test/tests/scanout_churn_retention.rs). The row is ABSENT, not zero-valued,
# when the worker holds none — an unguarded parse yields an empty string that reads like a failed
# measurement, so normalise it to 0. `graphics` is excluded the same way the L2 test excludes it.
owned_unmapped() {
  local v
  v=$(vmmap -summary "$(worker_pid)" 2>/dev/null \
      | grep "owned unmapped" | grep -v graphics | awk '{print $3}' | head -1)
  echo "${v:-0}"
}

# The census is object-exact: one leaked IOSurface is visible regardless of byte noise. It ticks
# on the ALLOCATION path, so it only refreshes while something is allocating.
census() { grep -a "refs — iosurface" "$WORKER_LOG" | tail -1 | sed 's/.*refs — //'; }

# macOS `wc -l` pads with spaces; `tail -n +<padded>` rejects it as an illegal offset.
mark() { wc -l < "$WORKER_LOG" | tr -d ' '; }
since() { tail -n "+$1" "$WORKER_LOG"; }

report() {
  local path="$1" from="$2"
  echo "  owned unmapped : $(owned_unmapped)"
  echo "  census         : $(census)"
  echo "  --- host lines for this path ---"
  # The destroy line is the discriminator: it says whether the instance was live (full sweep) or
  # gone (bare free()), and how much survived the sweep.
  since "$from" | grep -aE "destroying context|destroyed —|destroyed with|import res|still holds" \
    | sed 's/.*virgl: vkr: /    /' | sed 's/.*virgl_renderer\] /    /'
}

start_comp() {
  local extra="$1"
  g "sudo -n pkill -f 'limina-testcomp run'" ; sleep 1
  # shellcheck disable=SC2029
  "${SSH[@]}" "cd /tmp && sudo -n env VK_DRIVER_FILES=$ICD RUST_LOG=info nohup \
     ./limina-testcomp run $extra > /tmp/comp.log 2>&1 &" >/dev/null 2>&1 &
  sleep 4
  g "grep -a 'COMPOSITOR READY' /tmp/comp.log" || { echo "!! compositor did not start"; g 'tail -5 /tmp/comp.log'; return 1; }
}

echo "=== M3b teardown matrix (client size $SIZE) ==="
echo

# ---------------------------------------------------------------------------------------------
# Path 1 — clean exit. The client destroys its images, device and instance on the way out.
# Baseline: residual must be zero. Note this is ALSO the path that can leave orphans behind,
# since a destroyed instance is what sends vkr down the bare-free() branch for anything left.
# ---------------------------------------------------------------------------------------------
echo "--- path 1: clean exit ---"
start_comp "" || exit 1
BEFORE=$(owned_unmapped); FROM=$(mark)
echo "  before         : $BEFORE"
g "cd /tmp && sudo -n env VK_DRIVER_FILES=$ICD RUST_LOG=info XDG_RUNTIME_DIR=/tmp/testcomp-run \
   WAYLAND_DISPLAY=wayland-1 ./limina-testcomp client-dmabuf 8 $SIZE" | grep -E "CLIENT (COMMITTED|DONE)" | sed 's/^/  /'
sleep 3
report "clean" "$FROM"
echo "  compositor     : $(g 'grep -ac "evicted a client dmabuf" /tmp/comp.log') eviction(s)"
echo

# ---------------------------------------------------------------------------------------------
# Path 2 — abrupt exit (SIGKILL), with the compositor holding the buffer so the kill lands with
# it committed and unreleased. Per the correction above, expect the FULL sweep here.
# ---------------------------------------------------------------------------------------------
echo "--- path 2: SIGKILL, buffer committed and unreleased ---"
start_comp "--hold-buffers" || exit 1
BEFORE=$(owned_unmapped); FROM=$(mark)
echo "  before         : $BEFORE"
"${SSH[@]}" "cd /tmp && sudo -n env VK_DRIVER_FILES=$ICD RUST_LOG=info XDG_RUNTIME_DIR=/tmp/testcomp-run \
   WAYLAND_DISPLAY=wayland-1 ./limina-testcomp client-dmabuf 60 $SIZE --park" >/tmp/m3b-client.log 2>&1 &
sleep 6
grep -aE "CLIENT (COMMITTED|PARKED)" /tmp/m3b-client.log | sed 's/^/  /'
g "sudo -n pkill -9 -f 'limina-testcomp client-dmabuf'"
echo "  SIGKILLed"
sleep 4
report "sigkill" "$FROM"
echo "  compositor     : $(g 'grep -ac "evicted a client dmabuf" /tmp/comp.log') eviction(s)"
echo

# ---------------------------------------------------------------------------------------------
# Path 2b — the IMPORTER dies. The borrowed +1 lives on the compositor's side
# (mem->imported_iosurface), so killing the *client* only ever tests exporter-side death. This is
# the side that matches the observed compositor-quit residual.
# ---------------------------------------------------------------------------------------------
echo "--- path 2b: SIGKILL the COMPOSITOR while it holds a live import ---"
start_comp "--hold-buffers" || exit 1
BEFORE=$(owned_unmapped); FROM=$(mark)
echo "  before         : $BEFORE"
"${SSH[@]}" "cd /tmp && sudo -n env VK_DRIVER_FILES=$ICD RUST_LOG=info XDG_RUNTIME_DIR=/tmp/testcomp-run \
   WAYLAND_DISPLAY=wayland-1 ./limina-testcomp client-dmabuf 60 $SIZE --park" >/tmp/m3b-client2.log 2>&1 &
sleep 6
g "sudo -n pkill -9 -f 'limina-testcomp run'"
echo "  compositor SIGKILLed (client still alive, holding its export)"
sleep 4
report "importer-death" "$FROM"
g "sudo -n pkill -9 -f 'limina-testcomp client-dmabuf'"
sleep 3
echo "  after the client goes too:"
echo "  owned unmapped : $(owned_unmapped)"
echo

echo "=== done ==="
echo "Read the 'destroying context' lines above: 'instance was live' means the full sweep ran"
echo "(so vkr_device_memory_release dropped the +1); 'instance was gone' means the bare-free()"
echo "branch took whatever was left. A residual with a live instance is the interesting result."
