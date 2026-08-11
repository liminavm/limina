#!/bin/bash
# 10s sampler for the retention testbed: worker ledger + guest meminfo + balloon actual.
# Usage: sampler.sh <worker-pid> <balloon-sock> <ssh-port> <out.csv>
# Columns are in GiB except the raw guest KiB fields. Guest columns go empty (not
# zero) when ssh fails — a dead guest must never read as zero. Ledger columns use
# `ledger-dump -a` so entries that are genuinely zero (e.g. internal_compressed
# before any host pressure) print 0.000 instead of vanishing from the output.
set -u
pid=$1 sock=$2 port=$3 out=$4
dump="$(dirname "$0")/../hv-ledger-gap/ledger-dump"
echo "ts,ic_bal_g,ic_cred_g,ic_deb_g,int_bal_g,reus_bal_g,reus_cred_g,pf_g,stored_pages,balloon_actual_b,g_total_kib,g_free_kib,g_avail_kib,g_cached_kib" > "$out"
while kill -0 "$pid" 2>/dev/null; do
    ts=$(date +%s)
    row=$("$dump" "$pid" -a 2>/dev/null | awk '
        /^internal_compressed /{ic=$4; icc=$6; icd=$8}
        /^internal /{ib=$4}
        /^reusable /{rb=$4; rc=$6}
        /^phys_footprint /{pf=$4}
        END{printf "%s,%s,%s,%s,%s,%s,%s", ic, icc, icd, ib, rb, rc, pf}')
    stored=$(vm_stat | awk '/stored in compressor/{gsub("\\.","",$5); print $5}')
    actual=$(printf 'stats\n' | nc -U "$sock" -w 2 2>/dev/null | tr ' ' '\n' | awk -F= '/^actual/{print $2}')
    guest=$(ssh -o ConnectTimeout=3 -o BatchMode=yes -p "$port" claude@127.0.0.1 \
        "awk '/^MemTotal|^MemFree|^MemAvailable|^Cached:/{printf \"%s,\", \$2}' /proc/meminfo" 2>/dev/null)
    echo "$ts,$row,$stored,${actual:-},${guest%,}" >> "$out"
    sleep 10
done
