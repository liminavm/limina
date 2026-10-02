#!/bin/bash
# One MANAGED boot (release app binary, the Debian VM's own vm.toml + state.toml) of the LUKS
# clone to its passphrase prompt; record frames + console, then force-stop. Usage: miter.sh <n> [secs]
set -u
R=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
H=$R/spikes/console-redraw-repro
D=${CONREDRAW_DIR:-$H/work.noindex}
APP=$R/target/Limina.app/Contents/MacOS/limina
B=$D/lib/DebianRepro.liminavm
n=$1; secs=${2:-35}
I=$D/m$n; mkdir $I || { echo "m$n exists"; exit 1; }; mkdir $I/frames
cp $D/state.seed.toml $B/state.toml
find $B/run -maxdepth 1 -name 'snapshot.bin*' -delete 2>/dev/null
export LIMINA_VM_LIBRARY=$D/lib
export RUST_LOG=warn,limina=info,krun::vmm=info,krun_devices=info
export LIMINA_DISPLAY_TRACE=1 LIMINA_PRESENT_COPY_TRACE=1 LIMINA_GPU_TRACE=1
export LIMINA_WINDOW_CAPTURE=$I/capture.png LIMINA_WINDOW_CAPTURE_INTERVAL_MS=250
t0=$(python3 -c 'import time;print(time.time())')
$APP start DebianRepro --console $I/console.log > $I/start.out 2>&1 &
sup=$!
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
LIMINA_VM_LIBRARY=$D/lib $APP stop --force DebianRepro > $I/stop.out 2>&1
for k in $(seq 1 30); do kill -0 $sup 2>/dev/null || break; sleep 1; done
kill -0 $sup 2>/dev/null && kill -9 $sup
wait $sup 2>/dev/null
cp $I/start.out $I/worker.log
sleep 2
echo "m$n: frames=$(ls $I/frames | wc -l | tr -d ' ') last=$(ls $I/frames | tail -n 1)"
