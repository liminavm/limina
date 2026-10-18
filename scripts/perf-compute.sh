#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# The compute half of the perf battery (perf/README.md): GPU compute in a guest over the Metal
# transport, against the same work run natively on macOS straight after. Three tiers:
#   metal  the transport's own costs, metal-bench.cpp through limina-metal (commit round trip,
#          dispatch and command-buffer rates, copy bandwidth, two queues side by side)
#   mlx    MLX's pinned fork: matmuls, and mlx-lm generating with Qwen2.5 0.5B and 7B (4-bit)
#   llama  llama.cpp's Vulkan backend on the guest's KosmicKrisp: llama-bench pp512/tg128 on the
#          same two models as GGUF Q4_0
# Each guest row is followed by its native row and an `of_native` row: guest speed over native
# speed, so 1.00 is native and 0.80 is 20% slower. Trend rows, not a gate, like perf-ledger.sh's.
#
# It owns its boot: a fresh clone of metal.test (8 vCPUs, 16 GiB, LIMINA_GPU_METAL=1, the
# compositor stopped) with a clone of the tools disk as its second disk, then the native runs once
# the guest is down, because both use the one GPU. The tools disk is made once by
# scripts/perf/compute/tools-disk.sh; the native builds by scripts/perf/compute/native.sh, which
# this runs (it rebuilds only what moved). Run it from a tree whose debug worker is built.
#
# Usage: scripts/perf-compute.sh [notes]
#   PERF_TIERS   tiers to run (default "metal mlx llama")
#   PERF_RUNS    metal-bench runs per side (default 3; MLX and llama-bench repeat inside)
#   PERF_IMAGE   the guest image (default the main checkout's Fedora-Workstation-44.metal.test.raw)
#   PERF_EVIDENCE  where the logs go (default $PERF_HOME/runs/<date>-<commit>-<n>)
#   LIMINA_PERF_LEDGER  as perf-ledger.sh's (default perf/ledger.csv)
#   PERF_ALLOW_BUSY=1   measure although another VM is running (the notes then say so)
set -uo pipefail
NOTES="${1:-}"
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
cd "$ROOT" || exit 2
. scripts/perf/compute/env.sh
MAIN=$(cd "$(git rev-parse --path-format=absolute --git-common-dir)/.." && pwd)
TIERS=${PERF_TIERS:-metal mlx llama}
RUNS=${PERF_RUNS:-3}
IMAGE=${PERF_IMAGE:-$MAIN/Fedora-Workstation-44.metal.test.raw}
LEDGER=${LIMINA_PERF_LEDGER:-perf/ledger.csv}
DATE=$(date +%Y-%m-%d)
COMMIT=$(git rev-parse --short HEAD)
KK_ICD=${LIMINA_KK_ICD:-/Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.aarch64.json}
WORK=$PERF_HOME/work.noindex
# Fixed names, so nothing below deletes a computed path.
GUEST=$WORK/compute-guest.raw
TOOLS=$WORK/compute-tools.raw
n=0; while [ -e "$PERF_HOME/runs/$DATE-$COMMIT-$n" ]; do n=$((n + 1)); done
EV=${PERF_EVIDENCE:-$PERF_HOME/runs/$DATE-$COMMIT-$n}
mkdir -p "$EV" "$WORK"
[ -f "$LEDGER" ] || echo "date,commit,workload,metric,value,notes" > "$LEDGER"
log() { echo "[$(date +%H:%M:%S)] $*" | tee -a "$EV/run.log"; }

[ -f "$PERF_HOME/tools.raw" ] || { echo "no tools disk: scripts/perf/compute/tools-disk.sh" >&2; exit 2; }
[ -e "$KK_ICD" ] || { echo "no host KosmicKrisp ICD at $KK_ICD (scripts/ensure-mesa-cs.sh)" >&2; exit 2; }
others=$(pgrep -fl '[l]imina-vmm' | grep -v -- "$GUEST" || true)
if [ -n "$others" ]; then
  [ "${PERF_ALLOW_BUSY:-0}" = 1 ] || { echo "another VM is running; PERF_ALLOW_BUSY=1 to measure anyway:" >&2; echo "$others" >&2; exit 2; }
  NOTES="${NOTES:+$NOTES; }ANOTHER VM WAS RUNNING"
fi

log "building: limina-metal and the native control"
spikes/metal-cpp-shim/build.sh > "$EV/build.log" 2>&1 && scripts/perf/compute/native.sh >> "$EV/build.log" 2>&1 ||
  { log "ABORT: build (see $EV/build.log)"; exit 3; }

L=$(git -C third_party/libkrun rev-parse --short HEAD); V=$(git -C third_party/virglrs rev-parse --short HEAD)
M=$(git -C /Volumes/mesa-cs/mesa rev-parse --short HEAD 2>/dev/null)
X=$(git -C "$MLX_TREE" rev-parse --short HEAD)
# No commas: the notes column is quoted, but keep it readable as one cell.
PROV="libkrun $L; virglrs $V; host mesa $M; mlx $X; llama.cpp ${LLAMA_CPP_REV:0:8}; mlx-lm $MLX_LM; $(basename "$IMAGE"); macOS $(sw_vers -productVersion)"
log "$PROV"

# Host GPU utilisation through the run: the compute equivalent of reading pixels. A guest row
# taken while the GPU sat idle did not run where it says it did.
( while :; do
    echo "$(date +%H:%M:%S) $(ioreg -r -d 1 -c IOAccelerator | grep -o '"Device Utilization %"=[0-9]*' | head -n 1)"
    sleep 2
  done ) > "$EV/gpu-utilisation.txt" 2>&1 &
SAMPLER=$!
trap 'kill $SAMPLER 2>/dev/null' EXIT

# --- the guest
kill_by_disk() {
  for p in $(pgrep -f "[l]imina.*--disk $GUEST"); do
    log "killing our own VM: $(ps -o pid,lstart,command -p "$p" | tail -n 1)"; kill "$p"
  done
  for _ in $(seq 1 20); do pgrep -f "[l]imina.*--disk $GUEST" >/dev/null || return 0; sleep 1; done
  for p in $(pgrep -f "[l]imina.*--disk $GUEST"); do kill -9 "$p"; done
}
rm -f "$WORK/compute-guest.raw" "$WORK/compute-tools.raw"
cp -c "$IMAGE" "$GUEST" && cp -c "$PERF_HOME/tools.raw" "$TOOLS" || { log "ABORT: clones"; exit 3; }
log "guest boot"
env LIMINA_GPU_METAL=1 LIMINA_KK_ICD="$KK_ICD" LIMINA_CPUS=8 LIMINA_RAM_MIB=16384 LIMINA_NET=1 \
  LIMINA_EXTRA_ARGS="--disk $TOOLS" RUST_LOG=warn,limina=info,krun::vmm=info,krun_devices=info \
  LIMINA_DISK="$GUEST" spikes/venus-draw-probe/boot-enhanced-efi-kk.sh > "$EV/boot.txt" 2>&1 &
BPID=$!
WLOG=/tmp/limina-worker-compute-guest.log
if PORT=$(scripts/wait-guest-ssh.sh "$WLOG" 400 "$BPID"); then
  O=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR)
  SSH=(sshpass -p "$GUEST_PASS" ssh -p "$PORT" "${O[@]}" "$GUEST_USER@127.0.0.1")
  gtimeout --kill-after=10 60 "${SSH[@]}" 'mkdir -p /tmp/pc' &&
  COPYFILE_DISABLE=1 tar --no-mac-metadata --no-xattrs -cf - -C scripts/perf/compute guest-run.sh mlx_bench.py llama_rows.py \
      -C "$ROOT/spikes/metal-cpp-shim/out" liblimina_metal.a |
    gtimeout --kill-after=10 120 "${SSH[@]}" 'tar -C /tmp/pc -xf -'
  log "guest tiers: $TIERS"
  gtimeout --kill-after=30 5400 "${SSH[@]}" "bash /tmp/pc/guest-run.sh $RUNS $TIERS" > "$EV/guest.txt" 2>&1
  log "guest exit $?: $(grep -c '^ROW' "$EV/guest.txt") rows; $(grep '^FAIL' "$EV/guest.txt" | tr '\n' ' ')"
  gtimeout 30 "${SSH[@]}" 'sudo systemctl poweroff' >/dev/null 2>&1
  for _ in $(seq 1 60); do kill -0 "$BPID" 2>/dev/null || break; sleep 3; done
else
  log "ABORT: the guest did not come up"
fi
kill -0 "$BPID" 2>/dev/null && { log "poweroff timed out"; kill_by_disk; }
wait "$BPID" 2>/dev/null
cp "$WLOG" "$EV/worker.log" 2>/dev/null
rm -f "$WORK/compute-guest.raw" "$WORK/compute-tools.raw"

# --- native, with the guest down
N=$PERF_HOME/native
: > "$EV/native.txt"
for tier in $TIERS; do
  case $tier in
  metal)
    for r in $(seq 1 "$RUNS"); do
      log "native metal-bench run $r/$RUNS"
      "$N/metal-bench" "$N/metal-bench.metallib" 2>&1 | sed "/^ROW/s/\$/ run=$r/" >> "$EV/native.txt"
    done ;;
  mlx)
    log "native mlx"
    ( cd /tmp && "$PERF_HOME/venv-native/bin/python3" -I "$ROOT/scripts/perf/compute/mlx_bench.py" matmul
      for m in qwen0.5b:mlx-qwen2.5-0.5b-4bit qwen7b:mlx-qwen2.5-7b-4bit; do
        "$PERF_HOME/venv-native/bin/python3" -I "$ROOT/scripts/perf/compute/mlx_bench.py" lm "$PERF_HOME/models/${m#*:}" "${m%%:*}"
      done ) >> "$EV/native.txt" 2>&1 ;;
  llama)
    for m in qwen0.5b:Qwen2.5-0.5B-Instruct-Q4_0.gguf qwen7b:Qwen2.5-7B-Instruct-Q4_0.gguf; do
      log "native llama-bench ${m%%:*}"
      VK_ICD_FILENAMES="$KK_ICD" "$PERF_HOME/llama-cpp-host/build/bin/llama-bench" \
          -m "$PERF_HOME/models/gguf/${m#*:}" -p 512 -n 128 -r 5 -o jsonl 2>&1 |
        python3 -I scripts/perf/compute/llama_rows.py "${m%%:*}" >> "$EV/native.txt"
    done ;;
  esac
done
kill $SAMPLER 2>/dev/null; wait $SAMPLER 2>/dev/null

# The same weights through the same MLX on the same GPU write the same text, guest or native.
for t in $(grep -h '^TEXT' "$EV/guest.txt" | cut -d' ' -f2); do
  grep -q "^TEXT $t " "$EV/native.txt" || log "TEXT MISMATCH: the guest wrote $t, which the native run did not"
done

# --- rows: guest, native and of_native per workload and metric; repeated metal-bench runs are
# reduced to their median.
before=$(wc -l < "$LEDGER")
python3 -I - "$EV/guest.txt" "$EV/native.txt" "$DATE" "$COMMIT" "$PROV${NOTES:+; $NOTES}" >> "$LEDGER" <<'EOF'
import statistics, sys
def rows(path):
    d = {}
    for line in open(path, errors="replace"):
        p = line.split()
        if len(p) >= 4 and p[0] == "ROW":
            d.setdefault((p[1], p[2]), []).append(float(p[3]))
    return {k: statistics.median(v) for k, v in d.items()}
guest, native = rows(sys.argv[1]), rows(sys.argv[2])
date, commit, notes = sys.argv[3:6]
lower_is_better = ("ms", "us_median", "us_p90", "ratio")
for (w, m), g in sorted(guest.items()):
    print(f'{date},{commit},{w}-metal,{m},{g:g},"{notes}"')
    if (w, m) in native and not m.endswith("stddev"):
        n = native[(w, m)]
        print(f'{date},{commit},{w}-native,{m},{n:g},"{notes}"')
        of = (n / g) if m in lower_is_better else (g / n)
        print(f'{date},{commit},{w}-of-native,{m},{of:.3f},"{notes}"')
EOF
# The evidence may be committed to a public repo: name the trees, not where they sit on this host.
for f in "$EV"/*; do
  sed -i '' -e "s#$ROOT#\$ROOT#g" -e "s#$MAIN#\$MAIN#g" -e "s#$HOME#~#g" "$f"
done
log "rows appended to $LEDGER; evidence in $EV"
tail -n +"$((before + 1))" "$LEDGER" | grep -- '-of-native,' | cut -d, -f3-5 | tee -a "$EV/run.log"
