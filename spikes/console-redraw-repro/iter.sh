#!/bin/bash
# One boot of the Debian LUKS clone to its passphrase prompt: record every presented frame
# (window capture, 250 ms) and the serial console, then kill it. Usage: iter.sh <n> [seconds]
set -u
R=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
H=$R/spikes/console-redraw-repro
D=${CONREDRAW_DIR:-$H/work.noindex}
n=$1; secs=${2:-75}
I=$D/i$n; mkdir $I || { echo "i$n exists"; exit 1; }; mkdir $I/frames
find $D -maxdepth 1 -name 'disk.raw.limina-suspend*' -delete
export RUST_LOG=warn,limina=info,krun::vmm=info,krun_devices=info
export LIMINA_DISPLAY_TRACE=1 LIMINA_PRESENT_COPY_TRACE=1 LIMINA_GPU_TRACE=1
export LIMINA_WINDOW_CAPTURE=$I/capture.png LIMINA_WINDOW_CAPTURE_INTERVAL_MS=250
export LIMINA_DISK=$D/disk.raw LIMINA_BOOT_LOG=$I/worker.log LIMINA_NET=0
export LIMINA_EXTRA_ARGS="--console $I/console.log ${EXTRA:-}"
cd $R
t0=$(python3 -c 'import time;print(time.time())')
spikes/venus-draw-probe/boot-enhanced-efi-kk.sh > $I/boot.out 2>&1 &
boot=$!
python3 - "$I" "$secs" "$t0" <<'PY'
import sys,time,hashlib
I,secs,t0=sys.argv[1],float(sys.argv[2]),float(sys.argv[3])
last=None; k=0
while time.time()-t0<secs:
    try:
        b=open(I+'/capture.png','rb').read()
        h=hashlib.md5(b).hexdigest()
        if h!=last and len(b)>100:
            last=h; k+=1
            open(f'{I}/frames/{k:03d}-{time.time()-t0:06.2f}.png','wb').write(b)
    except Exception: pass
    time.sleep(0.1)
PY
for p in $(ps -axo pid,command | grep -F "$D/disk.raw" | grep -v grep | awk '{print $1}'); do
  ps -o command= -p $p | grep -qF "$D/disk.raw" && kill -9 $p
done
wait $boot 2>/dev/null
sleep 2
echo "i$n: frames=$(ls $I/frames | wc -l | tr -d ' ') last=$(ls $I/frames | tail -n 1)"
