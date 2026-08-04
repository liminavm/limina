#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Is this boot's GPU context healthy, or has a virgl/venus context been POISONED?
#
# Why this exists (2026-08-04, spikes/vrend-texture-corruption/): when vrend rejects a command
# from a guest context it marks that context in error PERMANENTLY — every later SUBMIT_3D fails.
# The guest keeps running, apps keep "working", and glmark2 keeps reporting scores, because it
# counts its own frames: failed submissions INFLATE FPS rather than erroring. A poisoned boot
# looks like a fast boot. We measured virgl "winning" by ~2x purely because it was broken.
#
# So: run this before recording ANY graphics number, and before concluding a boot rendered
# correctly. Exit 0 = healthy, 1 = poisoned, 2 = could not tell.
#
#   scripts/check-gpu-context-health.sh [SSH_PORT] [WORKER_LOG]
#
# Defaults: port 2222, /tmp/enhanced-efi-kk-worker.log
#
# The benign baseline: a handful (<20) of dequeue errors right at boot is the known-harmless
# gst-plugin-scan CREATE_VIDEO_BUFFER probe (see memory limina-virgl-vrend-perf). Poisoning looks
# completely different — hundreds to thousands, continuing long after login, ~120/s host-side.
# Do NOT reuse that "known red herring" verdict without checking the RATE and the TIMESTAMPS.
set -u

PORT="${1:-2222}"
WORKER_LOG="${2:-/tmp/enhanced-efi-kk-worker.log}"
THRESHOLD="${LIMINA_POISON_THRESHOLD:-50}"

SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=5 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null)

guest_errors=$(ssh -p "$PORT" "${SSH_OPTS[@]}" claude@127.0.0.1 \
  'sudo journalctl -b -o cat 2>/dev/null | grep -c virtio_gpu_dequeue_ctrl_func' 2>/dev/null)

if ! [[ "$guest_errors" =~ ^[0-9]+$ ]]; then
  echo "UNKNOWN: could not read the guest journal over ssh -p $PORT" >&2
  exit 2
fi

host_rejects=0
host_reason=""
if [ -f "$WORKER_LOG" ]; then
  host_rejects=$(grep -cE 'Illegal command buffer|failed to dispatch' "$WORKER_LOG" 2>/dev/null || echo 0)
  # The originating cause is logged ONCE, then drowned by ~120 submit failures/second.
  host_reason=$(grep -E 'failed to dispatch' "$WORKER_LOG" 2>/dev/null \
    | sed 's/.*virgl: //' | sort | uniq -c | sort -rn | head -3)
fi

echo "guest virtio_gpu_dequeue_ctrl_func errors: $guest_errors  (benign boot baseline: <20)"
echo "host  rejected/illegal command lines:      $host_rejects"
[ -n "$host_reason" ] && { echo "host rejection reasons:"; echo "$host_reason"; }

if [ "$guest_errors" -gt "$THRESHOLD" ]; then
  cat >&2 <<EOF

POISONED: a GPU context is in a permanent error state on this boot.
Any graphics measurement from it is FICTION (failed submits inflate FPS), and any
"it rendered fine" judgement is unsafe. Reboot the VM and re-check before measuring.
Details: spikes/vrend-texture-corruption/RESULTS.md
EOF
  exit 1
fi

echo "OK: no context poisoning detected."
exit 0
