#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# The guest half of scripts/perf-compute.sh, in a fresh clone of metal.test with a clone of the
# tools disk as its second disk. perf-compute.sh copies this script, mlx_bench.py, llama_rows.py
# and this tree's liblimina_metal.a to /tmp/pc first. Prints `ROW <name> <metric> <value>` lines
# (the host labels them) between commentary; a tier that cannot prove it is on the transport prints
# FAIL and no rows.
# Usage: guest-run.sh <runs> <tier>...   (tiers: metal mlx llama)
set -uo pipefail
RUNS=$1; shift
T=/opt/perf-compute
export PATH=/usr/bin:/usr/sbin
log() { echo "== $(date +%H:%M:%S) $*"; }

sudo mkdir -p $T
mountpoint -q $T || sudo mount /dev/disk/by-id/virtio-disk1 $T || { echo "FAIL: no tools disk"; exit 1; }
sudo chown claude: $T
# No compositor drawing on the GPU while we measure.
sudo systemctl isolate multi-user.target
sleep 5

# This tree's limina-metal, relinked into the one place every consumer loads it from.
gcc -shared -o $T/lib/liblimina_metal.so -Wl,--whole-archive /tmp/pc/liblimina_metal.a \
  -Wl,--no-whole-archive -lpthread -ldl -lm -lgcc_s || { echo "FAIL: relink limina-metal"; exit 1; }
cp /tmp/pc/mlx_bench.py $T/bench/
export LD_LIBRARY_PATH=$T/lib

echo "PROVENANCE kernel=$(uname -r) pagesize=$(getconf PAGESIZE) vcpus=$(nproc) ram=$(awk '/MemTotal/{print int($2/1024)"MiB"}' /proc/meminfo) wire=$(cat /opt/limina/*/WIRE_VERSION 2>/dev/null | tr '\n' ' ')limina-metal.a=$(sha256sum /tmp/pc/liblimina_metal.a | cut -c1-16)"
head -n 3 $T/MANIFEST | sed 's/^/MANIFEST /'

# The newest protocol the vendor drivers carry, as perf-ledger.sh picks it.
ICD=$(ls /etc/vulkan/icd.d/limina-*.json 2>/dev/null | sort -V | tail -n 1)

settle() {
  for _ in $(seq 1 40); do
    case $(cut -d' ' -f1 /proc/loadavg) in 0.0*|0.1*|0.2*) break ;; esac
    sleep 5
  done
  echo "load $(cut -d' ' -f1-3 /proc/loadavg)"
}
settle

for tier in "$@"; do
  case $tier in
  metal)
    for r in $(seq 1 "$RUNS"); do
      log "metal-bench run $r/$RUNS"
      out=$($T/bench/metal-bench $T/bench/metal-bench.metallib 2>&1); echo "$out" | grep -v '^ROW'
      # A guest without the transport has no Metal device at all; one named without (limina) is
      # not limina-metal.
      if ! echo "$out" | grep -q '^DEVICE .*(limina)'; then echo "FAIL: metal-bench is not on limina-metal"; continue; fi
      echo "$out" | grep '^ROW' | sed "s/\$/ run=$r/"
    done
    ;;
  mlx)
    . $T/venv/bin/activate
    cd /tmp || exit 1
    # MLX on its CPU backend would print plausible numbers; only limina-metal's device counts.
    mlx() {
      local out; out=$(python3 -I $T/bench/mlx_bench.py "$@" 2>&1 | grep -v 'os error')
      echo "$out" | grep -v '^ROW'
      if ! echo "$out" | grep -q '^DEVICE .*(limina)'; then echo "FAIL: MLX is not on limina-metal"; return; fi
      echo "$out" | grep '^ROW'
    }
    log "mlx matmul"
    mlx matmul
    for m in qwen0.5b:mlx-qwen2.5-0.5b-4bit qwen7b:mlx-qwen2.5-7b-4bit; do
      log "mlx-lm ${m%%:*}"
      mlx lm $T/models/${m#*:} ${m%%:*}
    done
    deactivate
    ;;
  llama)
    log "llama.cpp on $ICD"
    for m in qwen0.5b:Qwen2.5-0.5B-Instruct-Q4_0.gguf qwen7b:Qwen2.5-7B-Instruct-Q4_0.gguf; do
      log "llama-bench ${m%%:*}"
      out=$(VK_DRIVER_FILES=$ICD $T/llama.cpp/build/bin/llama-bench -m $T/models/gguf/${m#*:} \
              -p 512 -n 128 -r 5 -o jsonl 2>&1)
      echo "$out" | grep -v '^{'
      if ! echo "$out" | grep -q 'ggml_vulkan: 0 = .*KosmicKrisp'; then echo "FAIL: llama.cpp is not on KosmicKrisp"; continue; fi
      echo "$out" | grep '^{' | python3 -I /tmp/pc/llama_rows.py "${m%%:*}"
    done
    ;;
  *) echo "FAIL: unknown tier $tier" ;;
  esac
done
log "done"
