#!/bin/bash
# net-sweep.sh — sum every queryable task's NET compressor charge and compare
# with the system's stored count. The UNATTRIBUTED remainder (stored − sum,
# minus a small root-task slack) is the orphaned-slot pool. This is the
# correct per-scenario leak oracle: raw stored-after is polluted by bystander
# processes squeezed during a run (still alive, still billed); the leak is
# specifically compressed content NO live task carries.
# Usage: net-sweep.sh [label]   (one summary line to stdout; -v adds top 10)
# NOTE: the per-pid sweep takes ~15-20s while the vm_stat read is instant, so
# under heavy churn the two views skew and small NEGATIVE unattributed values
# (-0.1G-ish) are expected timing artifacts, not impossibilities. Only the
# bracket samples (quiesced host) carry verdict weight.
set -u
cd "$(dirname "$0")"
[ -x ./ledger-dump ] || clang -O2 -o ledger-dump ledger-dump.c || exit 1
label="${1:-}"
list=$(for pid in $(ps -axo pid=); do
    ./ledger-dump "$pid" 2>/dev/null | awk -v pid="$pid" '
        /^internal_compressed /            {ic=$4}
        /^alternate_accounting_compressed/ {ac=$4}
        /^tagged_footprint_compressed/     {tc=$4}
        /^purgeable_nonvolatile_compress/  {pc=$4}
        /^graphics_footprint_compressed/   {gc=$4}
        END {if (ic != "" || tc != "") printf "%.3f %s\n", ic-ac+tc+pc+gc, pid}'
done | sort -rn)
sum=$(echo "$list" | awk '{s+=$1} END {printf "%.3f", s}')
stored=$(vm_stat | awk '/stored in compressor/ {gsub("\\.","",$5); print $5}')
stored_g=$(echo "$stored" | awk '{printf "%.3f", $1*16384/1073741824}')
unattr=$(echo "$stored_g $sum" | awk '{printf "%.3f", $1-$2}')
echo "net-sweep${label:+ [$label]}: stored=${stored}p (${stored_g}G) live_net_sum=${sum}G unattributed=${unattr}G"
if [ "${2:-}" = "-v" ] || [ "${1:-}" = "-v" ]; then
    echo "$list" | head -10
fi
