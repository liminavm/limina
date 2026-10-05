#!/bin/bash
# In-guest battery for one arm: timer lateness at three periods, low- and high-duty chunk speed,
# futex-wake cost after an idle gap, and Wayland frame pacing in the seated session.
# Runs from ~/probes (built from probes/). Prints one summary line per measurement.
#
#   measure.sh <label>
set -uo pipefail
cd ~/probes
L="${1:?label}"

pct() { # file -> p50 p90 p99 max of a column of integers (ns), printed in µs
    sort -n "$1" | awk '{a[NR]=$1} END{printf "p50=%.0f p90=%.0f p99=%.0f max=%.0f n=%d", a[int(NR*.5)]/1e3, a[int(NR*.9)]/1e3, a[int(NR*.99)]/1e3, a[NR]/1e3, NR}'
}

sleep 20 # let the session settle after login before measuring

for period in 16667 4000 1000; do
    n=$((10000000 / period)); ((n > 2000)) && n=2000
    ./timerlat "$period" "$n" >/tmp/tl.txt
    echo "$L timerlat period_us=$period $(pct /tmp/tl.txt)"
done

for busy in 300 10000; do
    # Each line: elapsed_us:chunk_ns pairs; take the chunk times after 100 µs into the burst.
    ./dutyprobe 5000 60 "$busy" | tr ' ' '\n' | awk -F: 'NF==2 && $1>=100 {print $2}' | sort -n >/tmp/dp.txt
    echo "$L dutyprobe gap_us=5000 busy_us=$busy chunk_ns $(awk '{a[NR]=$1} END{printf "p50=%d p10=%d p90=%d n=%d", a[int(NR*.5)], a[int(NR*.1)], a[int(NR*.9)], NR}' /tmp/dp.txt)"
done

./wakecost 2 5 5000 600 >/tmp/wc.txt
echo "$L wakecost gap_us=5000 futex_wake $(awk '{print $1}' /tmp/wc.txt | sort -n | awk '{a[NR]=$1} END{printf "p50=%.1fus p90=%.1fus", a[int(NR*.5)]/1e3, a[int(NR*.9)]/1e3}') getppid $(awk '{print $2}' /tmp/wc.txt | sort -n | awk '{a[NR]=$1} END{printf "p50=%.2fus", a[int(NR*.5)]/1e3}')"

export XDG_RUNTIME_DIR=/run/user/$(id -u)
while IFS='=' read -r key value; do
    case "$key" in WAYLAND_DISPLAY | DBUS_SESSION_BUS_ADDRESS) export "$key=$value" ;; esac
done < <(systemctl --user show-environment)
fcprobe/fcprobe --seconds 10 --size 960x540 2>&1 | grep -E "^(frame_callback|presented|commit_to_present)" | sed "s/^/$L fcprobe /"
