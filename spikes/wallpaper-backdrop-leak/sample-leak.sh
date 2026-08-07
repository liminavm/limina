#!/bin/bash
# Leak sampler focused on the metric that actually moves: "owned unmapped".
# Fixes two degraded columns in the original sample-worker.sh — phys_footprint
# (vmmap prints "Physical footprint:  9.4G", not "="), and it adds the
# owned-unmapped size/region split. RSS is kept only to show it lying.
set -u
PID=$1; OUT=$2; INT=${3:-20}; DUR=${4:-900}
echo "ts,rss_kb,phys_footprint,vm_regions,ownunmap_size,ownunmap_regions,fds" > "$OUT"
END=$(( $(date +%s) + DUR ))
while kill -0 "$PID" 2>/dev/null && [ "$(date +%s)" -lt "$END" ]; do
    TS=$(date +%s)
    RSS=$(ps -o rss= -p "$PID" | tr -d ' ')
    SUM=$(vmmap -summary "$PID" 2>/dev/null || true)
    PHYS=$(echo "$SUM" | awk '/^Physical footprint:/ {print $3; exit}')
    REG=$(echo "$SUM" | awk '/^TOTAL /  {print $NF; exit}')
    OUSZ=$(echo "$SUM" | awk '/^owned unmapped  / {print $3; exit}')
    OUREG=$(echo "$SUM" | awk '/^owned unmapped  / {print $(NF); exit}')
    FDS=$(lsof -p "$PID" 2>/dev/null | wc -l | tr -d ' ')
    echo "$TS,$RSS,$PHYS,$REG,$OUSZ,$OUREG,$FDS" >> "$OUT"
    sleep "$INT"
done
echo "# done $(date +%s)" >> "$OUT"
