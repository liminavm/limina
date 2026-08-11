#!/bin/bash
# Retention-pool fix-grading testbed driver. See README.md.
# Usage: ./run.sh <disk.raw> [label]
# Env: MIX=full|touch|cache|none (default touch; cache = warm KEPT page cache + anon
#      churn — the clamp-binding profile: MemFree low, MemAvailable high), MEM (2G..12G),
#      CPUS (8), SSH_PORT (2233),
#      PLATEAU_MIN (16), SCRUB=1, KEEP_VM=0, SOAK_MIN=0 (no-scrub trickle soak:
#      idle SOAK_MIN, guest touch workload, idle SOAK_MIN again, ballast held),
#      POLICY_SCRUB=0 (live oracle for the supervisor's pressure-triggered scrub: boots
#      with the @file LIMINA_HOST_PRESSURE seam pinned "normal" + --reclaim SCRUB_MODE
#      plus the decision trace; after the other phases it waits out the mode's armed
#      scrub cooldown, flips the file to the mode's trigger level, and watches
#      trace.jsonl for one full start->hold->deflate->done cycle. Needs a guest running
#      limina-agent — no pressure reports, no policy ticks. Use with SCRUB=0.)
#      SCRUB_MODE=light|moderate|aggressive (default light — the only mode quiescent at
#      injected-Normal, so the pool build is undisturbed; moderate/aggressive inflate
#      during the plateau, pre-settling whatever the guest has free). Trigger/cooldown
#      per mode mirror balloon_policy.rs scrub_params: light=critical/60min,
#      moderate=warn/30min, aggressive=warn/15min. An agent that predates mem_free
#      reporting degrades bounded scrubs to eager-full — the oracle still cycles.
set -eu
SPIKE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$SPIKE/../.." && pwd)
GAP=$SPIKE/../hv-ledger-gap
DISK=${1:?usage: run.sh <disk.raw> [label]}
LABEL=${2:-run}
MIX=${MIX:-touch} MEM=${MEM:-2G..12G} CPUS=${CPUS:-8} SSH_PORT=${SSH_PORT:-2233}
PLATEAU_MIN=${PLATEAU_MIN:-16} SCRUB=${SCRUB:-1} KEEP_VM=${KEEP_VM:-0} SOAK_MIN=${SOAK_MIN:-0}
POLICY_SCRUB=${POLICY_SCRUB:-0} SCRUB_MODE=${SCRUB_MODE:-light}
# Per-mode scrub trigger + armed-cooldown wait (must mirror balloon_policy.rs scrub_params;
# +30 s of slack over the policy's cooldown so the flip lands past it, never just short).
case "$SCRUB_MODE" in
    light)      SCRUB_FLIP=critical SCRUB_WAIT=3630 ;;
    moderate)   SCRUB_FLIP=warn     SCRUB_WAIT=1830 ;;
    aggressive) SCRUB_FLIP=warn     SCRUB_WAIT=930 ;;
    *) echo "bad SCRUB_MODE=$SCRUB_MODE (light|moderate|aggressive)" >&2; exit 1 ;;
esac
STAMP=$(date +%s)
OUTDIR=$SPIKE/out-$LABEL-$STAMP
mkdir -p "$OUTDIR"
CSV=$OUTDIR/sampler.csv BOOTLOG=$OUTDIR/boot.log SOCK=$OUTDIR/balloon.sock
# macOS SUN_LEN caps Unix socket paths at ~104 bytes; a long label overflows it and the
# worker dies at bind time ("path must be shorter than SUN_LEN"). Fail early instead.
[ ${#SOCK} -le 100 ] || { echo "balloon socket path too long (${#SOCK} > 100): $SOCK — use a shorter label" >&2; exit 1; }
SSH="ssh -o BatchMode=yes -o ConnectTimeout=5 -o StrictHostKeyChecking=no -p $SSH_PORT claude@127.0.0.1"

log() { echo "[$(date +%H:%M:%S)] $*" | tee -a "$OUTDIR/driver.log"; }
[ -x "$GAP/ledger-dump" ] || clang -O2 -o "$GAP/ledger-dump" "$GAP/ledger-dump.c"
[ -x "$GAP/ballast" ] || clang -O2 -o "$GAP/ballast" "$GAP/ballast.c"
for f in "$ROOT/target/debug/limina" "$ROOT/target/debug/limina-vmm" "$ROOT/target/krun-efi/KRUN_EFI.gop.fd"; do
    [ -e "$f" ] || { echo "missing $f (cargo xtask build first)" >&2; exit 1; }
done

balloon() { printf '%s\n' "$1" | nc -U "$SOCK" -w 2 2>/dev/null; }
actual_bytes() { balloon stats | tr ' ' '\n' | awk -F= '/^actual/{print $2}'; }
last_col() { tail -1 "$CSV" | cut -d, -f"$1"; }
pool_g() { # ic_bal − guest live (MemTotal − MemAvailable), GiB
    tail -1 "$CSV" | awk -F, '{ printf "%.2f", $2 - ($14 - $16)/1048576 }'
}

cleanup() {
    [ -n "${BALLAST_PID:-}" ] && kill "$BALLAST_PID" 2>/dev/null || true
    [ -n "${SAMPLER_PID:-}" ] && kill "$SAMPLER_PID" 2>/dev/null || true
    if [ "$KEEP_VM" = 0 ] && [ -n "${VM_PID:-}" ] && kill -0 "$VM_PID" 2>/dev/null; then
        $SSH "sudo poweroff" 2>/dev/null || true
        for _ in $(seq 24); do kill -0 "$VM_PID" 2>/dev/null || break; sleep 5; done
        kill "$VM_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT

# --- Phase 0: boot ---------------------------------------------------------
RECLAIM_ARGS=()
if [ "$POLICY_SCRUB" = 1 ]; then
    echo normal > "$OUTDIR/host-pressure"
    export LIMINA_HOST_PRESSURE="@$OUTDIR/host-pressure"
    export LIMINA_BALLOON_TRACE="$OUTDIR/trace.jsonl"
    RECLAIM_ARGS=(--reclaim "$SCRUB_MODE") # light@Normal holds no balloon: quiescent until the flip
fi
log "booting $DISK (mem $MEM, $CPUS cpus) -> $BOOTLOG"
BOOT_TS=$(date +%s)
"$ROOT/target/debug/limina" --vmm-bin "$ROOT/target/debug/limina-vmm" \
    --firmware "$ROOT/target/krun-efi/KRUN_EFI.gop.fd" --disk "$DISK" \
    --net --ssh-port "$SSH_PORT" --cpus "$CPUS" --memory "$MEM" \
    --balloon-free-page-reporting --balloon-control-socket "$SOCK" \
    ${RECLAIM_ARGS[@]+"${RECLAIM_ARGS[@]}"} \
    > "$BOOTLOG" 2>&1 &
VM_PID=$!
for _ in $(seq 60); do $SSH true 2>/dev/null && break; sleep 5; done
$SSH true || { echo "guest never came up (see $BOOTLOG)" >&2; exit 1; }
WORKER_PID=$(pgrep -P "$VM_PID" -x limina-vmm || pgrep -x limina-vmm | head -1)
log "guest up; worker pid $WORKER_PID"
"$SPIKE/sampler.sh" "$WORKER_PID" "$SOCK" "$SSH_PORT" "$CSV" &
SAMPLER_PID=$!

# --- Phase 1: pool build ---------------------------------------------------
case $MIX in
full)  log "pool build: full compile mix"
       $SSH "bash ~/ab-run.sh" >> "$OUTDIR/mix.log" 2>&1 ;;
touch) log "pool build: synthetic dirty-then-free (cache + anon)"
       $SSH 'set -e
             dd if=/dev/urandom of=/var/tmp/pool.dat bs=1M count=4096 status=none
             cat /var/tmp/pool.dat > /dev/null
             rm /var/tmp/pool.dat
             python3 -c "
b = bytearray(3 << 30)
for i in range(0, len(b), 4096): b[i] = 1
" ' >> "$OUTDIR/mix.log" 2>&1 ;;
cache) log "pool build: warm KEPT cache (5G file, read twice) + 3G anon churn"
       $SSH 'set -e
             dd if=/dev/urandom of=/var/tmp/cache.dat bs=1M count=5120 status=none
             cat /var/tmp/cache.dat > /dev/null
             cat /var/tmp/cache.dat > /dev/null
             python3 -c "
b = bytearray(3 << 30)
for i in range(0, len(b), 4096): b[i] = 1
" ' >> "$OUTDIR/mix.log" 2>&1 ;;
none)  log "pool build: skipped" ;;
esac
log "idle 60s post-build"; sleep 60
log "post-build: ic_bal=$(last_col 2)G pool=$(pool_g)G"

# --- Phase 2: host pressure ------------------------------------------------
BALLAST_MIB=$(vm_stat | awk '
    /Pages free/{f=$3} /Pages inactive/{i=$3} /Pages speculative/{s=$3} /Pages purgeable/{p=$3}
    END{gsub("\\.","",f); gsub("\\.","",i); gsub("\\.","",s); gsub("\\.","",p)
        m=(f+i+s+p)*16384/1048576 - 2048; if (m>16384) m=16384; if (m<4096) m=4096; printf "%d", m}')
log "pressure: holding ${BALLAST_MIB}M incompressible ballast"
"$GAP/ballast" "$BALLAST_MIB" >> "$OUTDIR/ballast.log" 2>&1 &
BALLAST_PID=$!
prev=$(last_col 2); mins=0
while [ "$mins" -lt "$PLATEAU_MIN" ]; do
    sleep 120; mins=$((mins + 2))
    cur=$(last_col 2)
    log "  t+${mins}m ic_bal=${cur}G pool=$(pool_g)G"
    awk -v a="$prev" -v b="$cur" 'BEGIN{exit !(b-a < 0.2 && a-b < 0.2)}' && break
    prev=$cur
done
POOL_BEFORE=$(pool_g); IC_BEFORE=$(last_col 2)
log "MEASURE [$LABEL]: ic_bal=${IC_BEFORE}G pool=${POOL_BEFORE}G (repro = pool >= ~2G)"

# --- Phase 3: scrub grade --------------------------------------------------
if [ "$SCRUB" = 1 ]; then
    MAXB=$(echo "$MEM" | awk -F'\\.\\.' '{min=$1; max=$2; sub("G","",min); sub("G","",max)
                                          printf "%d", (max-min)*1073741824}')
    log "scrub: inflate to $MAXB bytes"
    balloon "target $MAXB" >/dev/null
    for _ in $(seq 36); do
        a=$(actual_bytes)
        [ -n "$a" ] && [ "$a" -ge $((MAXB * 9 / 10)) ] && break
        sleep 5
    done
    log "  inflated to $(actual_bytes) bytes; hold 30s"; sleep 30
    log "scrub: deflate to 0"
    balloon "target 0" >/dev/null
    for _ in $(seq 36); do
        a=$(actual_bytes)
        [ -n "$a" ] && [ "$a" -le $((MAXB / 20)) ] && break
        sleep 5
    done
    sleep 30
    POOL_AFTER=$(pool_g); IC_AFTER=$(last_col 2)
    log "SCRUB RESULT [$LABEL]: ic_bal ${IC_BEFORE}G -> ${IC_AFTER}G; pool ${POOL_BEFORE}G -> ${POOL_AFTER}G"
fi

# --- Phase 4: no-scrub trickle soak ---------------------------------------
if [ "$SOAK_MIN" -gt 0 ]; then
    soak() {
        local mins=0
        while [ "$mins" -lt "$1" ]; do
            sleep 120; mins=$((mins + 2))
            log "  soak-$2 t+${mins}m ic_bal=$(last_col 2)G pool=$(pool_g)G"
        done
    }
    log "SOAK idle ${SOAK_MIN}m (ballast held)"
    soak "$SOAK_MIN" idle
    log "SOAK activity: guest touch workload (2G cache cycle + 3G anon touch)"
    $SSH 'set -e
          dd if=/dev/urandom of=/var/tmp/soak.dat bs=1M count=2048 status=none
          cat /var/tmp/soak.dat > /dev/null
          rm /var/tmp/soak.dat
          python3 -c "
b = bytearray(3 << 30)
for i in range(0, len(b), 4096): b[i] = 1
" ' >> "$OUTDIR/mix.log" 2>&1
    log "SOAK post-activity: ic_bal=$(last_col 2)G pool=$(pool_g)G"
    soak "$SOAK_MIN" post
    log "SOAK RESULT [$LABEL]: ic_bal=$(last_col 2)G pool=$(pool_g)G (from ic_bal=${IC_BEFORE}G pool=${POOL_BEFORE}G at plateau)"
fi

# --- Phase 5: live policy-scrub oracle --------------------------------------
if [ "$POLICY_SCRUB" = 1 ]; then
    TRACE=$OUTDIR/trace.jsonl
    wait_trace() { # <ERE pattern> <timeout-sec>
        local t=0
        while [ "$t" -lt "$2" ]; do
            grep -qE "$1" "$TRACE" 2>/dev/null && return 0
            sleep 5; t=$((t + 5))
        done
        return 1
    }
    [ -s "$TRACE" ] || log "POLICY-SCRUB WARNING: trace empty — no pressure reports (agent not running?)"
    # The policy's scrub cooldown is armed at construction (mode-keyed): wait it out.
    while :; do
        UP=$(( $(date +%s) - BOOT_TS ))
        [ "$UP" -ge "$SCRUB_WAIT" ] && break
        log "POLICY-SCRUB: waiting out the armed scrub cooldown (uptime ${UP}s / ${SCRUB_WAIT}s)"
        sleep 60
    done
    PF_PRE=$(last_col 8); POOL_PRE=$(pool_g)
    log "POLICY-SCRUB: flipping injected host pressure to $SCRUB_FLIP (pf=${PF_PRE}G pool=${POOL_PRE}G)"
    # temp+rename: a read landing in the truncation window of a direct write pins Normal
    echo "$SCRUB_FLIP" > "$OUTDIR/host-pressure.tmp" && mv "$OUTDIR/host-pressure.tmp" "$OUTDIR/host-pressure"
    if ! wait_trace '"scrub":"start"' 180; then
        log "POLICY-SCRUB FAILED [$LABEL]: no scrub start within 180s of $SCRUB_FLIP (see $TRACE)"
    else
        log "POLICY-SCRUB: cycle started; waiting for a terminal event"
        wait_trace '"scrub":"(done|abort|watchdog)"' 420 \
            || log "POLICY-SCRUB: no terminal event within 420s"
        sleep 60
        PF_POST=$(last_col 8); POOL_POST=$(pool_g)
        log "POLICY-SCRUB RESULT [$LABEL]: pf ${PF_PRE}G -> ${PF_POST}G; pool ${POOL_PRE}G -> ${POOL_POST}G"
        grep '"scrub"' "$TRACE" | while IFS= read -r l; do log "  $l"; done
    fi
    echo normal > "$OUTDIR/host-pressure.tmp" && mv "$OUTDIR/host-pressure.tmp" "$OUTDIR/host-pressure"
fi
log "done; data in $OUTDIR"
