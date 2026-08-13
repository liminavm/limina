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
nohup "$REPO/spikes/venus-draw-probe/boot-enhanced-efi-kk.sh" > "$OUT/worker.log" 2>&1 &
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

WORKER=$(pgrep -f "limina-vmm.*soak.raw" | head -1)
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
cat > "$OUT/sample.sh" <<SAMPLER
#!/bin/bash
# One row every 2 min: the pool and what it is made of, plus the guest's own flip counters.
OUT="$OUT"; WORKER=$WORKER; SSH="$SSH"; HOURS=$HOURS
echo "ts,elapsed_min,footprint_mb,gfx_mb,gfx_regions,iosurface_mb,owned_unmapped_mb,total_regions,guest_flips,guest_created" > "\$OUT/soak.csv"
START=\$(date +%s)
END=\$(( START + HOURS * 3600 ))
while [ "\$(date +%s)" -lt "\$END" ]; do
    kill -0 "\$WORKER" 2>/dev/null || { echo "worker \$WORKER gone" >> "\$OUT/soak.err"; break; }
    F=\$(/usr/bin/footprint "\$WORKER" 2>/dev/null)
    fp=\$(echo "\$F"  | awk '/Footprint:/{print \$4}')
    gfx=\$(echo "\$F" | awk '/IOAccelerator \(graphics\)/{print \$1; exit}')
    gfxr=\$(echo "\$F"| awk '/IOAccelerator \(graphics\)/{for(i=1;i<=NF;i++) if(\$i=="IOAccelerator"){print \$(i-1); exit}}')
    ios=\$(echo "\$F" | awk '/IOSurface/{print \$1; exit}')
    own=\$(echo "\$F" | awk '/Owned physical footprint \(unmapped\) \(graphics\)/{print \$1; exit}')
    rgn=\$(echo "\$F" | awk '/Owned physical footprint \(unmapped\) \(graphics\)/{for(i=1;i<=NF;i++) if(\$i=="Owned"){print \$(i-1); exit}}')
    tail=\$(\$SSH "tail -1 /tmp/kmschurn.log" 2>/dev/null | tr -d '\r')
    flips=\$(echo "\$tail"   | grep -o 'flips=[0-9]*'   | cut -d= -f2)
    created=\$(echo "\$tail" | grep -o 'created=[0-9]*' | cut -d= -f2)
    now=\$(date +%s)
    echo "\$(date +%H:%M:%S),\$(( (now - START) / 60 )),\${fp:-},\${gfx:-},\${gfxr:-},\${ios:-},\${own:-},\${rgn:-},\${flips:-},\${created:-}" >> "\$OUT/soak.csv"
    sleep 120
done
echo "sampler done \$(date +%H:%M:%S)" >> "\$OUT/soak.csv"
SAMPLER
chmod +x "$OUT/sample.sh"
nohup "$OUT/sample.sh" > "$OUT/sampler.log" 2>&1 &
echo $! > "$OUT/soak.pid"

echo
echo "==> soak running for ${HOURS}h"
echo "    csv:    $OUT/soak.csv"
echo "    worker: $OUT/worker.log   (pid $WORKER, ssh port $SSH_PORT)"
echo "    stop:   kill \$(cat $OUT/soak.pid) && pkill -f 'limina-vmm.*soak.raw'"
