#!/bin/bash
# Package-power windows for the vCPU policy arms, read against a `powermetrics --samplers cpu_power
# -i 1000` capture someone runs as root alongside. This script only marks the windows: every
# phase start and end goes to <outdir>/phases.txt as "<epoch> <local time> <label> <begin|end>".
#
#   power-arms.sh <outdir> <reps> [arm...]       arms as in run-arms.sh (default off band off+lat0)
#
# Per rep: no VM for BASE_S (default 120). Then per arm: boot, wait for ssh, settle 30 s, an idle
# window (IDLE_S, default 120), an animating window (fcprobe redrawing a 960x540 window for ANIM_S,
# default 60), and power off.
set -euo pipefail
cd "$(dirname "$0")/../.."
out="$(mkdir -p "${1:?outdir}" && cd "$1" && pwd)"
reps="${2:?reps}"
shift 2
arms=("$@")
[ ${#arms[@]} -gt 0 ] || arms=(off band off+lat0)
BASE_S="${BASE_S:-120}" IDLE_S="${IDLE_S:-120}" ANIM_S="${ANIM_S:-60}"
disk="${LIMINA_DISK:-vcpu-qos-poke.raw}"
log="/tmp/limina-worker-$(basename "${disk%.raw}").log"
ssh_opts=(-o BatchMode=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR)
mark() { echo "$(date +%s) $(date '+%T') $1 $2" >>"$out/phases.txt"; }

for rep in $(seq 1 "$reps"); do
    mark "base-r$rep" begin; sleep "$BASE_S"; mark "base-r$rep" end
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
        sleep 30
        mark "$label-idle" begin; sleep "$IDLE_S"; mark "$label-idle" end
        mark "$label-anim" begin
        ssh -p "$port" "${ssh_opts[@]}" claude@127.0.0.1 "export XDG_RUNTIME_DIR=/run/user/\$(id -u); \
            eval \$(systemctl --user show-environment | grep -E '^(WAYLAND_DISPLAY|DBUS_SESSION_BUS_ADDRESS)=' | sed 's/^/export /'); \
            probes/fcprobe/fcprobe --seconds $ANIM_S --size 960x540" 2>&1 | grep -E "^presented:" >"$out/$label.fcprobe.txt" || true
        mark "$label-anim" end
        grep -a "VCPU-RT" "$log" >"$out/$label.vcpu-rt.txt" || true
        ssh -p "$port" "${ssh_opts[@]}" claude@127.0.0.1 "sudo systemctl poweroff" >/dev/null 2>&1 || true
        wait "$boot" || true
        echo "$(date '+%T') $label done: $(/bin/cat "$out/$label.fcprobe.txt")"
        sleep 5
    done
done
mark all end
