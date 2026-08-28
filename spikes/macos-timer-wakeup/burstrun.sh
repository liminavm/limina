#!/bin/bash
# How much does a BURST of full guest occupancy cost a presenting client?
#   burstrun.sh <label> <burst-seconds> <period-seconds>
#
# Sustained saturation is already known to destroy a fully banded guest. The open question is the
# transition: dynamic arming samples every 200 ms, so a build starting flips every vCPU busy while
# all of them are still banded, and nobody has measured whether a window that short does damage.
# So: run vkcube, and every <period> seconds give every vCPU a spinner for <burst> seconds.
#
# Reported as the frame-time tail rather than average FPS — the bursts are meant to cost frames,
# and the question is whether they cost *far more* than their own duration.
set -u
label=$1; burst=$2; period=$3
export XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0
rm -rf /tmp/mh; mkdir -p /tmp/mh
pkill -f vkcube 2>/dev/null; pkill -f spin-load 2>/dev/null; sleep 1

MANGOHUD_CONFIG="fps,frame_timing,log_duration=20,output_folder=/tmp/mh,autostart_log=1" \
  mangohud vkcube --width 900 --height 600 >/tmp/vkcube.log 2>&1 &
vk=$!
sleep 3   # let it reach a steady cadence before the first burst

bursts=0
end=$((SECONDS + 20))
while [ "$SECONDS" -lt "$end" ]; do
  for i in $(seq "$(nproc)"); do
    (exec -a spin-load timeout "$burst" bash -c 'while :; do :; done') &
  done
  bursts=$((bursts + 1))
  sleep "$period"
done
kill $vk 2>/dev/null; pkill -f spin-load 2>/dev/null; sleep 1

log=$(ls -t /tmp/mh/*.csv 2>/dev/null | grep -v _summary | head -1)
[ -z "$log" ] && { echo "$label burst=${burst}s: NO LOG (is mangohud installed?)"; exit 1; }
python3 - "$log" "$label" "$burst" "$bursts" <<'PY'
import sys, csv
path, label, burst, bursts = sys.argv[1:5]
rows = list(csv.reader(open(path)))
start = next((i + 1 for i, r in enumerate(rows) if r and r[0].strip() == 'fps'), 0)
ft = []
for r in rows[start:]:
    if len(r) < 2: continue
    try:
        v = float(r[1])          # ms
        if v > 0: ft.append(v)
    except ValueError: pass
if not ft:
    print(f"{label} burst={burst}s: NO SAMPLES"); sys.exit(1)
ft.sort()
def p(q): return ft[min(len(ft) - 1, int(len(ft) * q))]
over = lambda t: sum(1 for f in ft if f > t)
print(f"{label:10s} burst={burst}s x{bursts:2s} n={len(ft):5d} "
      f"avgFPS={1000/(sum(ft)/len(ft)):5.1f} p50={p(.50):6.2f} p99={p(.99):7.2f} "
      f"max={ft[-1]:8.2f} >33ms={over(33):4d} >100ms={over(100):3d} >500ms={over(500):3d} "
      f"stalled={sum(f for f in ft if f > 100)/1000:5.2f}s")
PY
