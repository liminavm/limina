#!/bin/bash
# The cells left open after the base-M1 runs, from a release bundle, once the host has no other VM:
#   A  stock vehicle, 8 vCPUs   (band + tier 0 on a quiet host)
#   B  enhanced vehicle, 4 vCPUs
#   C  enhanced vehicle, 2 vCPUs
# All four arms in each, 3 reps, 60 s of fcprobe per boot; no power windows.
#
#   fps-cells.sh <Limina.app> <outdir>     run from the repo root
set -euo pipefail
app="${1:?Limina.app}"
out="${2:?outdir}"
while pgrep -f '[l]imina-vmm --cpus' >/dev/null; do sleep 120; done
echo "$(date '+%T') host has no VM; starting"
export WAIT_SSH=scripts/wait-guest-ssh.sh BASE_S=0 IDLE_S=0 ANIM_S=60
arms=(off band off+lat0 band+lat0)
LIMINA_CPUS=8 spikes/guest-vcpu-qos/power-arms-bundle.sh "$app" vcpu-qos-poke.raw "$out/stock-8cpu" 3 "${arms[@]}"
LIMINA_CPUS=4 spikes/guest-vcpu-qos/power-arms-bundle.sh "$app" vcpu-qos-enh.raw "$out/enh-4cpu" 3 "${arms[@]}"
LIMINA_CPUS=2 spikes/guest-vcpu-qos/power-arms-bundle.sh "$app" vcpu-qos-enh.raw "$out/enh-2cpu" 3 "${arms[@]}"
echo "$(date '+%T') all cells done"
