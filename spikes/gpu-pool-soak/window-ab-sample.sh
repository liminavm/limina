#!/bin/bash
# Sample both ends of the graphics pool on one clock, for window-lifecycle A/B runs.
#
# WHY BOTH ENDS. Host footprint(1) says what the WORKER holds; it cannot say what the GUEST
# believes is live, so a host-only reading cannot separate "the guest is holding more" from
# "the host is not retiring". Pairing them is what made the 2026-08-13 teardown decisive.
#
# WHY A SCRIPT. Round 1 of the ghost A/B was scored from a hand-rolled sampler and could not be
# scored at all: the churn arm ended with a window still open, so the "residue" was a live
# window. It also showed the region count breathing across 649 regions with the guest blob count
# pinned -- so a run needs enough samples at each idle point to average through that, which means
# a fixed cadence and a long enough window, not an ad-hoc loop.
#
# Read the result with a segment mean at each ZERO-WINDOW idle point, never a single sample:
# comparing two idles with different guest blob counts compares different live sets, which is
# exactly how round 1 went void.
#
# Usage: window-ab-sample.sh <vm-host> <guest-port> <guest-user> <out.csv> [minutes] [interval-s]
set -u
VMHOST=${1:?ssh host running the VM}
PORT=${2:?guest ssh forward port}
GUSER=${3:?guest user}
OUT=${4:?output csv}
MINUTES=${5:-45}
INTERVAL=${6:-15}

echo "ts,gfx_mb,gfx_regions,iosurface_mb,guest_blobs,guest_blob_mb,guest_fb,guest_clients" > "$OUT"
END=$(( $(date +%s) + MINUTES * 60 ))
while [ "$(date +%s)" -lt "$END" ]; do
    # -n goes on the INNER ssh only. The inner one would otherwise consume the remainder of this
    # heredoc from `sh -s`'s stdin, truncating every later line; but -n on the OUTER ssh redirects
    # its stdin from /dev/null, so the heredoc never arrives at all and every row comes back empty.
    row=$(ssh -o ConnectTimeout=20 "$VMHOST" PORT="$PORT" GUSER="$GUSER" sh -s <<'RS' 2>/dev/null
PID=$(pgrep -x limina-vmm | head -1)
[ -n "$PID" ] || exit 0
F=$(/usr/bin/footprint "$PID" 2>/dev/null)
# Size cells carry their own unit and it CHANGES as the pool grows -- normalize to MB so a
# column never silently changes meaning mid-run.
MB='{v=$1; u=$2; if(u=="GB") v*=1024; else if(u=="KB") v/=1024; else if(u=="B") v/=1048576; printf "%.0f", v; exit}'
gfx=$(echo "$F" | awk "/IOAccelerator \(graphics\)/$MB")
rgn=$(echo "$F" | awk '/IOAccelerator \(graphics\)/{for(i=1;i<=NF;i++) if($i=="IOAccelerator"){print $(i-1); exit}}')
ios=$(echo "$F" | awk "/IOSurface/$MB")
g=$(ssh -n -p "$PORT" -o BatchMode=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
       -o ConnectTimeout=8 "$GUSER@127.0.0.1" 'sudo -n sh -c "D=/sys/kernel/debug/dri/0
u=\$(grep -c \": used\$\" \$D/virtio-gpu-host-visible-mm 2>/dev/null)
m=\$(awk -F: \"/: used\\\$/ {s+=\\\$2} END{printf \\\"%.0f\\\", s/1048576}\" \$D/virtio-gpu-host-visible-mm 2>/dev/null)
f=\$(grep -c \"^framebuffer\[\" \$D/framebuffer 2>/dev/null)
c=\$(tail -n +2 \$D/clients 2>/dev/null | grep -c .)
echo \"\$u \$m \$f \$c\""' 2>/dev/null | tail -1)
set -- $g
echo "${gfx:-},${rgn:-},${ios:-},${1:-},${2:-},${3:-},${4:-}"
RS
)
    echo "$(date +%H:%M:%S),${row:-,,,,,,}" >> "$OUT"
    sleep "$INTERVAL"
done
echo "sampler done $(date +%H:%M:%S)" >> "$OUT"
