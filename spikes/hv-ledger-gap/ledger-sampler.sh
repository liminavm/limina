#!/bin/bash
# Read-only sampler for the limina-vmm compressor-ledger investigation.
# Appends one CSV row per 5 min to /tmp/limina-ledger-trace.csv, plus an
# hourly vmmap-summary attributable reading to /tmp/limina-ledger-vmmap.log.
# Purely observational; kill with: pkill -f limina-ledger-sampler
PIDFILE=/tmp/limina-ledger-sampler.pid
if [ -f "$PIDFILE" ] && kill -0 "$(cat "$PIDFILE")" 2>/dev/null; then
    echo "already running ($(cat "$PIDFILE"))" >&2
    exit 0
fi
echo $$ > "$PIDFILE"
OUT=/tmp/limina-ledger-trace.csv
VMOUT=/tmp/limina-ledger-vmmap.log
[ -s "$OUT" ] || echo "ts,pid,ic_bal_g,ic_cred_g,ic_deb_g,int_bal_g,reus_bal_g,reus_cred_g,pf_g,stored_pages,occupied_pages,seg_total,seg_swapped,swap_used_mb" > "$OUT"
n=0
while true; do
    ts=$(date +%s)
    pid=$(pgrep -x limina-vmm | head -1)
    if [ -n "$pid" ]; then
        row=$(/tmp/ledger-dump "$pid" 2>/dev/null | awk '
            /^internal_compressed /{ic=$4; icc=$6; icd=$8}
            /^internal /{ib=$4}
            /^reusable /{rb=$4; rc=$6}
            /^phys_footprint /{pf=$4}
            END{printf "%s,%s,%s,%s,%s,%s,%s", ic, icc, icd, ib, rb, rc, pf}')
    else
        row=",,,,,,"
    fi
    sys=$(vm_stat | awk '/stored in compressor/{gsub("\\.","",$5); s=$5}
                         /occupied by compressor/{gsub("\\.","",$5); o=$5}
                         END{printf "%s,%s", s, o}')
    segs=$(sysctl -n vm.compressor.segment.total vm.compressor.segment.swappedout 2>/dev/null | paste -sd, -)
    swap=$(sysctl -n vm.swapusage | awk '{gsub("M","",$6); print $6}')
    echo "$ts,${pid:-none},$row,$sys,$segs,$swap" >> "$OUT"
    if [ $((n % 12)) -eq 0 ] && [ -n "$pid" ]; then
        {
            echo "== $(date '+%F %T') pid=$pid"
            vmmap --summary "$pid" 2>/dev/null | grep -E "Physical footprint|Writable regions"
        } >> "$VMOUT"
    fi
    n=$((n + 1))
    sleep 300
done
