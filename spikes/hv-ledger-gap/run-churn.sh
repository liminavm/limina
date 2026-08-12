#!/bin/bash
# run-churn.sh — drive one churn-probe leg with pre/post system measurement.
# The verdict readout is the POST-EXIT residue: if "Pages stored in compressor"
# does not return to the pre-run baseline (minus normal noise), the orphan leak
# reproduced. Usage: ./run-churn.sh <label> [churn-probe args...]
set -u
cd "$(dirname "$0")"
label="${1:?usage: run-churn.sh <label> [probe args...]}"
shift

if pgrep -x limina-vmm >/dev/null || pgrep -f "nextest|balloon_bench" >/dev/null; then
    echo "REFUSING: an HVF bench/VM is running on this machine; pressure would" >&2
    echo "contaminate it (and it contaminates our segment counts)." >&2
    exit 1
fi

sys() {
    vm_stat | awk '/stored in compressor/ {gsub("\\.","",$5); s=$5}
                   /occupied by compressor/ {gsub("\\.","",$5); o=$5}
                   END {printf "stored=%d occupied=%d ", s, o}'
    sysctl -n vm.compressor.segment.total vm.compressor.segment.swappedout |
        paste -sd' ' - | awk '{printf "segs=%s segswap=%s ", $1, $2}'
    sysctl -n vm.swapusage
}

out="churn-${label}-$(date +%s).log"
echo "== leg: $label  args: $*" | tee "$out"
echo "== pre  $(sys)" | tee -a "$out"
./churn-probe "$@" 2>&1 | tee -a "$out"
rc=${PIPESTATUS[0]}
echo "== probe exited rc=$rc; settling 10s" | tee -a "$out"
sleep 10
echo "== post $(sys)" | tee -a "$out"
echo "== post+60s follow-up (does the residue decay?)" | tee -a "$out"
sleep 60
echo "== post60 $(sys)" | tee -a "$out"
echo "log: $out"
