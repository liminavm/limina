#!/bin/bash
# Guest-side arm of the GPU-pool soak — the half the host-side sampler cannot see.
#
# WHY. Host `footprint(1)` says how much the WORKER holds. It cannot say how much the GUEST
# believes is live, so a growing host pool is ambiguous: the guest may genuinely be holding more
# resources, or the host may be failing to retire ones the guest released. The 2026-08-13 dogfood
# curve ran straight into this — the pool tripled over a day, and the natural objection ("apps
# accumulated") could not be tested, because nothing measured the guest's live set. Closing two
# idle apps then dropped it 22% in four minutes, which fits BOTH stories.
#
# So sample both ends on the same clock and compare TRENDS, not ratios (the absolute numbers are
# not comparable — the host pool includes Metal heaps and driver-side allocations the guest has no
# notion of):
#
#   guest flat + host flat    -> retention healthy
#   guest grows + host grows  -> the guest really is holding more; not a host bug
#   guest flat + host grows   -> HOST-SIDE RETENTION. This is the finding worth having.
#
# Sources, all under /sys/kernel/debug/dri/0 (root-only, hence sudo -n):
#   virtio-gpu-host-visible-mm  the guest driver's own used/free map of host-visible blobs —
#                               the closest guest analogue to the host's region count
#   framebuffer                 live DRM framebuffers (scanouts the guest has registered)
#   clients                     DRM clients, to catch a process leaking whole connections
#
# Usage: guest-arm.sh <out-dir> <ssh-port> [hours]
set -u
OUT=${1:?out dir}; PORT=${2:?ssh port}; HOURS=${3:-24}
SSH="ssh -p $PORT -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 claude@127.0.0.1"

PROBE='sudo -n sh -c "
fb=\$(grep -c \"^framebuffer\[\" /sys/kernel/debug/dri/0/framebuffer 2>/dev/null)
used=\$(grep -c \": used\$\" /sys/kernel/debug/dri/0/virtio-gpu-host-visible-mm 2>/dev/null)
free=\$(grep -c \": free\$\" /sys/kernel/debug/dri/0/virtio-gpu-host-visible-mm 2>/dev/null)
ub=\$(awk -F: \"/: used\\\$/ {s+=\\\$2} END{printf \\\"%.0f\\\", s/1048576}\" /sys/kernel/debug/dri/0/virtio-gpu-host-visible-mm 2>/dev/null)
cl=\$(tail -n +2 /sys/kernel/debug/dri/0/clients 2>/dev/null | grep -c .)
echo \"\$fb \$used \$free \$ub \$cl\"
"'

echo "ts,elapsed_min,guest_fb,guest_hv_used,guest_hv_free,guest_hv_used_mb,guest_drm_clients" > "$OUT/guest.csv"
START=$(date +%s)
END=$(( START + HOURS * 3600 ))
while [ "$(date +%s)" -lt "$END" ]; do
    read -r fb used free ub cl <<<"$($SSH "$PROBE" 2>/dev/null | tr -d '\r')"
    now=$(date +%s)
    echo "$(date +%H:%M:%S),$(( (now - START) / 60 )),${fb:-},${used:-},${free:-},${ub:-},${cl:-}" >> "$OUT/guest.csv"
    sleep 120
done
echo "guest arm done $(date +%H:%M:%S)" >> "$OUT/guest.csv"
