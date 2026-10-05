#!/bin/bash
# Boot the vehicle once per arm (a vCPU thread picks its policy at start), run measure.sh in the
# guest, power it off. Arms interleave within each rep.
#
#   run-arms.sh <outdir> <reps> [arm...]
#     arm: off | band | off+lat0 | band+lat0   (band = the shipped default, rt+dyn#1)
# Env: LIMINA_DISK (default vcpu-qos-poke.raw, a CoW clone of the stock test image with gcc,
# wayland-devel and ~/probes built), LIMINA_CPUS (8).
set -euo pipefail
cd "$(dirname "$0")/../.."
out="$(mkdir -p "${1:?outdir}" && cd "$1" && pwd)"
reps="${2:?reps}"
shift 2
arms=("$@")
[ ${#arms[@]} -gt 0 ] || arms=(off band off+lat0 band+lat0)
disk="${LIMINA_DISK:-vcpu-qos-poke.raw}"
log="/tmp/limina-worker-$(basename "${disk%.raw}").log"
ssh_opts=(-o BatchMode=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR)

for rep in $(seq 1 "$reps"); do
    for arm in "${arms[@]}"; do
        label="$arm-r$rep"
        case "$arm" in
        off) sched="" lat="" ;;
        band) sched="rt+dyn#1" lat="" ;;
        off+lat0) sched="" lat=0 ;;
        band+lat0) sched="rt+dyn#1" lat=0 ;;
        *) echo "unknown arm $arm" >&2; exit 2 ;;
        esac
        export LIMINA_VCPU_SCHED="$sched"
        if [ -n "$lat" ]; then export LIMINA_VCPU_LATENCY_QOS="$lat"; else unset LIMINA_VCPU_LATENCY_QOS; fi
        export RUST_LOG=warn,limina=info,krun=info LIMINA_DISK="$disk" LIMINA_CPUS="${LIMINA_CPUS:-8}" \
            LIMINA_RAM_MIB=8192 LIMINA_DISPLAY_CAPTURE="$out/$label.png"
        spikes/venus-draw-probe/boot-enhanced-efi-kk.sh >"$out/$label.boot.txt" 2>&1 &
        boot=$!
        port=$(scripts/wait-guest-ssh.sh "$log" 300)
        ssh -p "$port" "${ssh_opts[@]}" claude@127.0.0.1 "probes/measure.sh $label" >"$out/$label.txt" 2>&1 || true
        grep -a "VCPU-RT" "$log" >"$out/$label.vcpu-rt.txt" || true
        echo "$(date '+%T') $label: $(wc -l <"$out/$label.txt") lines"
        ssh -p "$port" "${ssh_opts[@]}" claude@127.0.0.1 "sudo systemctl poweroff" >/dev/null 2>&1 || true
        wait "$boot" || true
        sleep 5
    done
done
