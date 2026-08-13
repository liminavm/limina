#!/bin/sh
# One paired host+guest reading of the graphics pool. Read-only; safe against a dogfood VM.
#
# This is the oracle that settled the "is the pool leaking?" question on 2026-08-13. Either end
# alone is ambiguous — host footprint(1) says what the WORKER holds, which includes Metal heaps
# and driver allocations the guest has no notion of, so a growing pool fits both "the guest is
# holding more" and "the host is failing to retire". Sampling both on the same clock and watching
# them move together (or not) is what makes the reading decisive:
#
#   guest drops + host drops  -> retirement is healthy (what we measured: full DRM teardown took
#                                the pool to 16 regions within 20 s)
#   guest flat  + host grows  -> host-side retention
#
# Run it from the machine that can ssh the VM HOST. It ssh's on to the guest through the VM's own
# forward, so it needs the guest port (read it from supervisor.log's "guest SSH forward ready"
# line — it auto-allocates from 2222 up, do NOT assume 2222) and the guest login.
#
# Usage: paired-probe.sh <vm-host> <guest-ssh-port> [guest-user]
set -eu
VMHOST=${1:?ssh host running the VM}
PORT=${2:?guest ssh forward port}
GUSER=${3:-claude}

ssh "$VMHOST" PORT="$PORT" GUSER="$GUSER" sh -s <<'RS'
PID=$(pgrep -x limina-vmm | head -1)
[ -n "$PID" ] || { echo "worker=none (parked or stopped)"; exit 0; }
F=$(/usr/bin/footprint "$PID" 2>/dev/null)
# The region count is the column just before the category name, and the size cell carries its own
# unit — print both verbatim rather than normalizing, so a unit change is visible not silent.
printf 'host  %s pid=%s\n' "$(date +%H:%M:%S)" "$PID"
echo "$F" | grep -E 'Footprint:|IOAccelerator \(graphics\)|IOSurface' | sed 's/^/  /'

# Guest counters, all under /sys/kernel/debug/dri/0 and root-only:
#   virtio-gpu-host-visible-mm  the guest driver's used/free map of host-visible blobs
#   framebuffer / clients       live scanouts, and DRM clients (catches a leaked connection)
# -n so this nested ssh does not consume the outer `sh -s` script still queued on stdin.
ssh -n -p "$PORT" -o BatchMode=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
    -o ConnectTimeout=8 "$GUSER@127.0.0.1" 'sudo -n sh -c "
D=/sys/kernel/debug/dri/0
used=\$(grep -c \": used\$\" \$D/virtio-gpu-host-visible-mm 2>/dev/null)
free=\$(grep -c \": free\$\" \$D/virtio-gpu-host-visible-mm 2>/dev/null)
ub=\$(awk -F: \"/: used\\\$/ {s+=\\\$2} END{printf \\\"%.0f\\\", s/1048576}\" \$D/virtio-gpu-host-visible-mm 2>/dev/null)
fb=\$(grep -c \"^framebuffer\[\" \$D/framebuffer 2>/dev/null)
cl=\$(tail -n +2 \$D/clients 2>/dev/null | grep -c .)
echo \"guest hv_used=\$used hv_free=\$free hv_used_mb=\$ub fb=\$fb drm_clients=\$cl\"
"' 2>/dev/null | tail -1
RS
