#!/bin/bash
# 24 h GPU-pool soak: does the worker's IOAccelerator retention plateau, or grow without bound?
#
# WHY THIS EXISTS. On the dogfood Mac (2026-08-13) the worker's graphics pool climbed from
# 1.90 G / 9,579 regions to 3.64 G / 15,925 over a few hours — with the guest ostensibly idle —
# against the ~815 M the budget retirement is supposed to settle at (docs/design/gpu-memory-budget.md).
# A later reading fell back to 3.56 G / 15,488, so *something* retires; what we cannot tell from a
# few hours is whether the high-water mark is bounded. Three outcomes this is built to separate:
#
#   monotonic climb  -> retention with no effective retirement (a leak by another name)
#   sawtooth         -> retirement works; the question becomes what sets the high-water mark
#   plateau          -> bounded retention, working as designed; the dogfood number is just its size
#
# VEHICLE. `crates/limina-test/guest/kmschurn.py` in `churn-vk` mode: a ctypes KMS presenter that
# allocates a FRESH scanout buffer per flip, flips, and releases the previous — the shape
# spikes/venus-churn-retention/RESULTS.md reduced synoik to. It needs no compositor, no GDM and no
# seat: `systemctl isolate multi-user.target` frees DRM master and it takes the card directly.
# Page flips are vsync-paced, so this is a ~60 fps desktop-like load, not a benchmark hammer.
#
# `-vk` (direct Vulkan allocation), NOT the gbm mode, is deliberate: synoik allocates its scanout
# buffers Vulkan-natively now, so the gbm-under-zink path that spike wrote up is no longer the
# compositor's shape. The two modes differ in the allocator and nothing else — same modeset, same
# flips, same release discipline — so if this soak ever needs the other one, swapping the mode
# string is the whole change.
#
# The L2 test `scanout_churn_retention` runs the same vehicle for 300 frames as a regression gate;
# this is the same thing over ~5.2M frames to answer a question the short run cannot.
#
# Usage: spikes/gpu-pool-soak/soak.sh [hours]   (default 24)
# Output: spikes/gpu-pool-soak/run-<stamp>/{soak.csv,guest.log,worker.log,meta.txt}
#
# Runs detached from any Claude session (nohup'd sampler): the CSV is the deliverable and any
# session can read it. Stop early with: kill $(cat run-<stamp>/soak.pid)
set -euo pipefail

HOURS=${1:-24}
REPO=$(cd "$(dirname "$0")/../.." && pwd)
STAMP=$(date +%Y%m%d-%H%M%S)
OUT="$REPO/spikes/gpu-pool-soak/run-$STAMP"
mkdir -p "$OUT"

# The soak mutates its disk (it boots in place), so clone. APFS clones are instant and
# copy-on-write, so this costs no real disk until the guest writes.
SRC="$REPO/Fedora-Workstation-44.enhanced.test.raw"
DISK="$REPO/Fedora-Workstation-44.soak.raw"
[ -f "$SRC" ] || { echo "no source image at $SRC" >&2; exit 1; }
echo "==> cloning $SRC -> $DISK"
rm -f "$DISK"
cp -c "$SRC" "$DISK"

# 1280x800 matches the L2 test, so bytes-per-framebuffer (4 MB) are comparable with its numbers.
# 8 GiB guest: the subject is host-side region accumulation, not guest RAM, and the dev Mac is
# also the build machine — leaving headroom keeps it usable.
export LIMINA_DISK="$DISK"
export LIMINA_NET=1
export LIMINA_RAM_MIB=8192
export LIMINA_CPUS=4
# --display-resolution, NOT --display-size: the latter only backstops a screen-less host under
# --display-capture, and a windowed boot left to its `host` default follows whatever screen the
# window lands on. That would make bytes-per-framebuffer depend on the monitor and break every
# comparison with the L2 test's 4 MB-per-buffer numbers.
export LIMINA_EXTRA_ARGS="--display-resolution 1280x800"

{
    echo "stamp:       $STAMP"
    echo "hours:       $HOURS"
    echo "repo HEAD:   $(cd "$REPO" && git rev-parse --short HEAD)"
    echo "disk:        $DISK (clone of $(basename "$SRC"))"
    echo "vehicle:     kmschurn.py churn-vk, 1280x800, vsync-paced"
    echo "host:        $(sw_vers -productVersion) $(uname -m)"
} > "$OUT/meta.txt"

echo "==> booting the soak guest (EFI + venus on KosmicKrisp, windowed)"
# The worker's own log goes where LIMINA_BOOT_LOG says — the boot script otherwise uses one
# fixed /tmp path that every concurrent boot would share, and the SSH forward we need to parse
# is announced there, not on the script's stdout.
export LIMINA_BOOT_LOG="$OUT/worker.log"
nohup "$REPO/spikes/venus-draw-probe/boot-enhanced-efi-kk.sh" > "$OUT/boot.log" 2>&1 &
BOOT_PID=$!
echo "$BOOT_PID" > "$OUT/boot.pid"

# The supervisor prints the auto-allocated forward; never assume 2222.
echo "==> waiting for the guest SSH forward"
SSH_PORT=""
for _ in $(seq 1 120); do
    sleep 5
    SSH_PORT=$(grep -o 'ssh -p [0-9]*' "$OUT/worker.log" 2>/dev/null | tail -1 | awk '{print $3}')
    [ -n "$SSH_PORT" ] && break
done
[ -n "$SSH_PORT" ] || { echo "guest never advertised an SSH forward; see $OUT/worker.log" >&2; exit 1; }
echo "==> guest SSH on port $SSH_PORT"

SSH="ssh -p $SSH_PORT -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
     -o ConnectTimeout=10 claude@127.0.0.1"
for _ in $(seq 1 60); do
    $SSH true 2>/dev/null && break
    sleep 5
done

# The WORKER, not the supervisor. `pgrep -f "limina-vmm.*soak.raw"` also matches the supervisor
# — its argv names both the worker binary and the disk — and sampling that measures a ~16 MB
# process for 24 h. Match the process NAME exactly, then confirm the disk from its argv.
WORKER=""
for p in $(pgrep -x limina-vmm); do
    if ps -o command= -p "$p" | grep -q "soak.raw"; then WORKER=$p; break; fi
done
[ -n "$WORKER" ] || { echo "no worker pid for the soak disk" >&2; exit 1; }
echo "==> worker pid $WORKER"
echo "worker pid:  $WORKER" >> "$OUT/meta.txt"
echo "ssh port:    $SSH_PORT" >> "$OUT/meta.txt"

# Free DRM master for the presenter: the session compositor holds it, and nothing about the
# graphical target is the subject here.
echo "==> isolating to multi-user.target and staging the presenter"
$SSH "sudo -n systemctl isolate multi-user.target" || true
sleep 5
$SSH "cat > /tmp/kmschurn.py" < "$REPO/crates/limina-test/guest/kmschurn.py"

# ~60 fps, vsync-paced, for the requested span (plus slack so the guest side outlives the sampler).
FRAMES=$(( HOURS * 3600 * 60 + 100000 ))
VENUS_ENV="VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json"
echo "==> starting the churn presenter ($FRAMES frames)"
$SSH "nohup sudo -n env $VENUS_ENV python3 /tmp/kmschurn.py churn-vk $FRAMES 2 1280 800 \
      > /tmp/kmschurn.log 2>&1 & echo started"

# Host-side sampler. Detached, so it survives this shell and any Claude session.
#
# The body is a QUOTED heredoc (no shell expansion at all) with the run's values prepended as
# plain assignments. The unquoted form needed a backslash on every `$` and got one wrong on the
# first run — the CSV logged the literal string "Footprint:" as a number for two samples. Quoted
# + prepended config makes that class of bug impossible.
{
    echo '#!/bin/bash'
    echo "OUT=\"$OUT\""
    echo "WORKER=$WORKER"
    echo "SSH=\"$SSH\""
    echo "HOURS=$HOURS"
    cat "$REPO/spikes/gpu-pool-soak/sample-body.sh"
} > "$OUT/sample.sh"
chmod +x "$OUT/sample.sh"
nohup "$OUT/sample.sh" > "$OUT/sampler.log" 2>&1 &
echo $! > "$OUT/soak.pid"

echo
echo "==> soak running for ${HOURS}h"
echo "    csv:    $OUT/soak.csv"
echo "    worker: $OUT/worker.log   (pid $WORKER, ssh port $SSH_PORT)"
echo "    stop:   kill \$(cat $OUT/soak.pid) && pkill -f 'limina-vmm.*soak.raw'"
