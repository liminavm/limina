#!/bin/bash
# Measure vkcube's frame times under MangoHud for one arm.
#   fpsrun.sh <label> <idle|loaded|saturated>
#
# `loaded` leaves spare vCPUs; `saturated` runs one spinner per vCPU, so every vCPU thread has guest
# work at all times and never parks. Only the second shape can trip xnu's real-time fail-safe.
set -u
label=$1; mode=${2:-idle}
export XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0
rm -rf /tmp/mh; mkdir -p /tmp/mh
pkill -f 'vkcube' 2>/dev/null; pkill -f 'spin-load' 2>/dev/null; sleep 1

spinners=0
[ "$mode" = loaded ] && spinners=6
[ "$mode" = saturated ] && spinners=$(nproc)
if [ "$spinners" -gt 0 ]; then
  for i in $(seq "$spinners"); do
    (exec -a spin-load bash -c 'while :; do :; done') &
  done
  sleep 2
fi

MANGOHUD_CONFIG="fps,frame_timing,log_duration=20,output_folder=/tmp/mh,autostart_log=1" \
  mangohud vkcube --width 900 --height 600 >/tmp/vkcube.log 2>&1 &
vk=$!
sleep 26
kill $vk 2>/dev/null
pkill -f 'spin-load' 2>/dev/null
sleep 1

log=$(ls -t /tmp/mh/*.csv 2>/dev/null | grep -v _summary | head -1)
[ -z "$log" ] && { echo "$label $mode: NO LOG"; exit 1; }
python3 - "$log" "$label" "$mode" <<'PY'
import sys, csv
path, label, mode = sys.argv[1], sys.argv[2], sys.argv[3]
rows = list(csv.reader(open(path)))
# MangoHud writes a header line of its own before the column header.
start = 0
for i, r in enumerate(rows):
    if r and r[0].strip() == 'fps':
        start = i + 1
        break
ft = []
for r in rows[start:]:
    if len(r) < 2: continue
    try: ft.append(float(r[1]))  # frametime is in ms
    except ValueError: pass
ft = [f for f in ft if f > 0]
if not ft:
    print(f"{label} {mode}: NO SAMPLES"); sys.exit(1)
ft.sort()
def p(q): return ft[min(len(ft)-1, int(len(ft)*q))]
avg = sum(ft)/len(ft)
print(f"{label:14s} {mode:7s} n={len(ft):5d} avgFPS={1000/avg:5.1f} "
      f"p50={p(.50):7.2f} p90={p(.90):7.2f} p99={p(.99):8.2f} max={ft[-1]:8.2f}")
PY
