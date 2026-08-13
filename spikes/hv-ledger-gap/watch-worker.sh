#!/bin/sh
# Periodic read-only memory watch over a deployed limina worker on a remote Mac.
#
# Everything here comes from the deployed build's own instrumentation, so it needs no
# debug build and no changes on the target:
#   - the balloon decision trace (footprint_bytes/compressed_bytes since limina 663716d)
#   - ledger-dump (deployed to /tmp on the target; source in this directory)
#   - footprint(1) for the graphics pool
#
# The `gap` column is the demand sweep's own sensor — resident footprint past the guest's
# honest live share — so a reading at/above DEMAND_SWEEP_GAP (4 GiB) with the sweeps
# counter not advancing is the "trigger is wedged" tell. `faults` spinning to millions is
# the sweep fault-handler refault loop (see docs/hardening-backlog.md).
#
# Usage: LIMINA_WATCH_HOST=<ssh-host> [LIMINA_WATCH_VM=Dev] [LIMINA_WATCH_GUEST_GIB=24] \
#        [LIMINA_WATCH_INTERVAL=1200] spikes/hv-ledger-gap/watch-worker.sh
set -eu

HOST=${LIMINA_WATCH_HOST:?set LIMINA_WATCH_HOST to the ssh host running the VM}
VM=${LIMINA_WATCH_VM:-Dev}
GUEST_GIB=${LIMINA_WATCH_GUEST_GIB:-24}
INTERVAL=${LIMINA_WATCH_INTERVAL:-1200}

prev_sw=""
while true; do
    out=$(ssh -o ConnectTimeout=20 "$HOST" VM="$VM" GUEST_GIB="$GUEST_GIB" sh -s <<'RS' 2>/dev/null || true
PID=$(pgrep -x limina-vmm | head -1)
LOGD="$HOME/Library/Application Support/Limina/VMs/$VM.liminavm/logs"
if [ -z "$PID" ]; then echo "worker=none (parked or stopped)"; exit 0; fi
t=$(tail -1 "$LOGD/balloon-trace.jsonl" 2>/dev/null)
j() { echo "$t" | sed -n "s/.*\"$1\":\([0-9]*\).*/\1/p"; }
fp=$(j footprint_bytes); ic=$(j compressed_bytes); act=$(j actual_bytes)
sw=$(j sweeps); swdeb=$(j sweep_debited_bytes); swf=$(j sweep_faults)
psi=$(j some_avg10); hpct=$(j host_avail_pct); gfree=$(j free_kib)
host=$(echo "$t" | sed -n 's/.*"host":"\([a-z]*\)".*/\1/p')
guest=$((GUEST_GIB * 1024 * 1024 * 1024))
gap=$(( ${fp:-0} - ${ic:-0} - (guest - ${act:-0}) ))
[ "$gap" -lt 0 ] && gap=0
sc=$(grep -c '"scrub":"start"' "$LOGD/balloon-trace.jsonl" 2>/dev/null)
# Since limina 76ff982 the trace names the trigger that fired, so demand/cadence/idle pacing is
# READ rather than inferred from counter deltas. Absent on older builds -> these read 0.
dem=$(grep -c '"sweep":"demand"' "$LOGD/balloon-trace.jsonl" 2>/dev/null)
hold=$(grep -c '"sweep":"holdoff"' "$LOGD/balloon-trace.jsonl" 2>/dev/null)
idle=$(grep -c '"trigger":"idle"' "$LOGD/balloon-trace.jsonl" 2>/dev/null)
# Longest run of consecutive Hold(Cooldown) still standing: the settled-free lift (0a57407)
# should keep this short. A long run with free memory available is the defect it fixes.
cdrun=$(python3 - "$LOGD/balloon-trace.jsonl" 2>/dev/null <<'PY'
import json,sys
best=cur=0
for line in open(sys.argv[1]):
    try: d=json.loads(line)
    except Exception: continue
    if "decision" not in d: continue
    if d["decision"]=="cooldown" and d["free_kib"]*1024 > (1<<30):
        cur+=1; best=max(best,cur)
    else: cur=0
print(best)
PY
)
reus=$(/tmp/ledger-dump "$PID" 2>/dev/null | awk '/^reusable /{print $4}')
F=$(/usr/bin/footprint "$PID" 2>/dev/null)
gfx=$(echo "$F" | awk '/IOAccelerator \(graphics\)/{print $1$2; exit}')
gfxr=$(echo "$F" | awk '/IOAccelerator \(graphics\)/{for(i=1;i<=NF;i++) if($i=="IOAccelerator"){print $(i-1); exit}}')
ios=$(echo "$F" | awk '/IOSurface/{print $1$2; exit}')
echo "pf=$(( ${fp:-0} >> 20 ))M ic=$(( ${ic:-0} >> 20 ))M gap=$(( gap >> 20 ))M \
balloon=$(( ${act:-0} >> 30 ))G reus=${reus:-?}G | gfx=${gfx:-?}/${gfxr:-?}rgn iosurf=${ios:-?} \
| sweeps=${sw:-?}(dem=${dem:-0}) debit=$(( ${swdeb:-0} >> 20 ))M faults=${swf:-?} \
scrubs=${sc:-?}(idle=${idle:-0}) holdoff=${hold:-0} cd_run=${cdrun:-0} \
| guest_free=$(( ${gfree:-0} / 1024 ))M psi=${psi:-?} host=${host:-?}/${hpct:-?}%"
RS
    )
    [ -n "$out" ] || out="target unreachable (ssh failed)"
    sw_now=$(echo "$out" | sed -n 's/.*sweeps=\([0-9]*\).*/\1/p')
    note=""
    if [ -n "$prev_sw" ] && [ -n "$sw_now" ] && [ "$sw_now" -gt "$((prev_sw + 1))" ]; then
        note=" [demand-paced: +$((sw_now - prev_sw)) sweeps]"
    fi
    [ -n "$sw_now" ] && prev_sw=$sw_now
    echo "$(date +%H:%M) $out$note"
    sleep "$INTERVAL"
done
