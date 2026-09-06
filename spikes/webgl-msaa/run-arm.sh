#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# One arm of the WebGL {antialias:true} device-loss investigation, end to end:
# clone a pristine image, boot it, run the page in the seated session, and watch
# until the device is lost or the window expires.
#
# The failure is stochastic (spikes/webgl-msaa/RESULTS.md), so a single run of
# this proves nothing on its own -- run it k times per side and compare survival
# rates, or run it once to collect the device-loss report, which now names the
# command buffers in the failing commit.
#
# The guest is deliberately small. Every arm that dies ends in SIGABRT and
# strands host anonymous memory that only a reboot reclaims, so vm_stat is
# recorded either side of the run and printed at the end.
set -euo pipefail
cd "$(dirname "$0")/../.."

BASE="${LIMINA_BASE_IMAGE:-Fedora-Workstation-44.enhanced.test.raw}"
NAME="${1:-arm}"
OUT="${LIMINA_ARM_OUT:-/tmp/webgl-msaa-$NAME}"
WATCH="${LIMINA_ARM_WATCH:-360}"
# Workload bisection: appended to the page's query string, e.g. "&notex=1".
QUERY="${LIMINA_ARM_QUERY:-}"
CLONE="msaa-$NAME.raw"

mkdir -p "$OUT"
[ -f "$BASE" ] || { echo "no base image at $BASE" >&2; exit 77; }

compressor() { vm_stat | awk '/occupied by compressor/ {gsub(/\./,"",$5); print $5}'; }
echo "compressor before: $(compressor) pages" | tee "$OUT/vm-before.txt"

rm -f "$CLONE" "$CLONE.limina-suspend.bin"
cp -c "$BASE" "$CLONE"

WORKER="/tmp/limina-worker-msaa-$NAME.log"
rm -f "$WORKER"
LIMINA_DISK="$PWD/$CLONE" \
LIMINA_NET=1 \
LIMINA_RAM_MIB="${LIMINA_RAM_MIB:-3072}" \
LIMINA_CPUS="${LIMINA_CPUS:-4}" \
LIMINA_EXTRA_ARGS="--display-size 2560x1440 ${LIMINA_EXTRA_ARGS:-}" \
LIMINA_DISPLAY_CAPTURE="$OUT/capture.png" \
RUST_LOG="${RUST_LOG:-warn,limina=info,krun_vmm=info,krun_devices=info}" \
LIMINA_VREND_BLIT_LOG=1 \
    nohup spikes/venus-draw-probe/boot-enhanced-efi-kk.sh > "$OUT/boot.log" 2>&1 &
BOOT_PID=$!

PORT=$(scripts/wait-guest-ssh.sh "$WORKER" 300 "$BOOT_PID")
SSH=(ssh -p "$PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null claude@127.0.0.1)

scp -P "$PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
    spikes/webgl-msaa/webgl-msaa.html claude@127.0.0.1:/home/claude/webgl-msaa.html

# The session has to be up before a client can be launched into it: sshd answers
# its banner well before mutter has a compositor.
"${SSH[@]}" 'for i in $(seq 1 60); do
                 [ -S /run/user/1000/wayland-0 ] && exit 0
                 sleep 2
             done
             echo "no wayland socket after 120s" >&2; exit 1'

# Through the session manager, not a bare ssh command line: both inherit the same
# environment (measured, byte-identical), but this is the launch the desktop uses.
URL="file:///home/claude/webgl-msaa.html?aa=1${QUERY}"
echo "page URL: $URL"
"${SSH[@]}" "export XDG_RUNTIME_DIR=/run/user/1000 \
                    DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus
             rm -rf /tmp/ffarm && mkdir -p /tmp/ffarm
             systemd-run --user --unit=webglmsaa --collect /usr/bin/firefox \
                 --profile /tmp/ffarm --new-instance --kiosk \
                 '$URL'"

# The page must be shown reaching the GPU before a survival can be read: an arm
# whose browser never started looks exactly like a healthy one, and this has
# happened repeatedly -- a missing profile directory, a page never copied, a
# session not yet up. Two independent checks, and the arm is VOID if either
# fails, because a void arm reported as a survival is worse than no arm.
sleep 25
FF=$("${SSH[@]}" 'pgrep -c firefox' 2>/dev/null || echo 0)
echo "$FF" > "$OUT/firefox-count.txt"
echo "firefox processes after launch: $FF"

# The multisampled blit on the wire is the workload, not the browser: only it
# proves the antialiased canvas is actually rendering through vrend.
#
# LIMINA_ARM_EXPECT_MSAA=0 inverts that: with the host advertising max_samples=1
# the page still runs but takes the single-sample path, so an s4 blit must NOT
# appear, and requiring one would void every arm of the mitigation being tested.
EXPECT_MSAA="${LIMINA_ARM_EXPECT_MSAA:-1}"
for _ in $(seq 1 20); do
    grep -q 'LIMINA-BLIT.*s4' "$WORKER" && break
    sleep 3
done
S4=$(grep -c 'LIMINA-BLIT.*s4' "$WORKER" 2>/dev/null || echo 0)

if [ "$FF" -eq 0 ] || { [ "$EXPECT_MSAA" = 1 ] && [ "$S4" -eq 0 ]; } \
                   || { [ "$EXPECT_MSAA" = 0 ] && [ "$S4" -ne 0 ]; }; then
    cp "$WORKER" "$OUT/worker.log" 2>/dev/null || true
    cp "$OUT/capture.png" "$OUT/capture-live.png" 2>/dev/null || true
    PID=$(pgrep -f '[l]imina --vmm-bin' || true)
    [ -n "$PID" ] && kill "$PID" 2>/dev/null || true
    sleep 8
    rm -f "$CLONE" "$CLONE.limina-suspend.bin"
    echo "=== $NAME: VOID -- firefox=$FF, multisampled blit on the wire: $S4 (expected $EXPECT_MSAA)"
    echo "    (look at $OUT/capture.png; the workload never reached the GPU)"
    exit 75
fi

START=$(date +%s)
VERDICT=survived
while pgrep -f '[l]imina-vmm --cpus' > /dev/null; do
    if grep -q 'LIMINA-DEVICE-LOST' "$WORKER" 2>/dev/null; then VERDICT=lost; break; fi
    [ $(( $(date +%s) - START )) -ge "$WATCH" ] && break
    sleep 5
done
[ "$VERDICT" = survived ] && ! pgrep -f '[l]imina-vmm --cpus' > /dev/null && VERDICT=died-silently
ELAPSED=$(( $(date +%s) - START ))

cp "$WORKER" "$OUT/worker.log" 2>/dev/null || true
# Before the kill: stopping the supervisor shuts the guest down, and the capture
# is overwritten once a second, so the file left behind otherwise shows nothing
# but systemd stopping units -- for a survival and a death alike.
cp "$OUT/capture.png" "$OUT/capture-live.png" 2>/dev/null || true
PID=$(pgrep -f '[l]imina --vmm-bin' || true)
[ -n "$PID" ] && kill "$PID" 2>/dev/null || true
sleep 8
rm -f "$CLONE" "$CLONE.limina-suspend.bin"

echo "compressor after: $(compressor) pages" | tee "$OUT/vm-after.txt"
echo "=== $NAME: $VERDICT after ${ELAPSED}s (artefacts in $OUT)"
grep -A 30 'LIMINA-DEVICE-LOST' "$OUT/worker.log" 2>/dev/null || true
