#!/bin/bash
# power-arms.sh for a host with no limina checkout: boot from a Limina.app bundle's own `limina`
# instead of the dev tree's boot vehicle. Same windows and phases.txt format.
#
#   power-arms-bundle.sh <Limina.app> <disk> <outdir> <reps> [arm...]
# Needs wait-guest-ssh.sh beside it. BASE_S / IDLE_S / ANIM_S and LIMINA_CPUS as in power-arms.sh.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
app="${1:?Limina.app}"
disk="${2:?disk}"
out="$(mkdir -p "${3:?outdir}" && cd "$3" && pwd)"
reps="${4:?reps}"
shift 4
arms=("$@")
[ ${#arms[@]} -gt 0 ] || arms=(off band off+lat0)
BASE_S="${BASE_S:-120}" IDLE_S="${IDLE_S:-120}" ANIM_S="${ANIM_S:-60}"
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
        export RUST_LOG=warn,limina=info,krun=info
        log="$out/$label.worker.log"
        "$app/Contents/MacOS/limina" --firmware "$app/Contents/Resources/KRUN_EFI.gop.fd" \
            --disk "$disk" --cpus "${LIMINA_CPUS:-8}" --ram-mib 8192 --net \
            --display-capture "$out/$label.png" >"$log" 2>&1 &
        boot=$!
        port=$("$here/wait-guest-ssh.sh" "$log" 300 "$boot")
        sleep 30
        mark "$label-idle" begin; sleep "$IDLE_S"; mark "$label-idle" end
        mark "$label-anim" begin
        ssh -p "$port" "${ssh_opts[@]}" claude@127.0.0.1 "export XDG_RUNTIME_DIR=/run/user/\$(id -u); \
            eval \$(systemctl --user show-environment | grep -E '^(WAYLAND_DISPLAY|DBUS_SESSION_BUS_ADDRESS)=' | sed 's/^/export /'); \
            probes/fcprobe/fcprobe --seconds $ANIM_S --size 960x540" 2>&1 | grep -E "^presented:" >"$out/$label.fcprobe.txt" || true
        mark "$label-anim" end
        ssh -p "$port" "${ssh_opts[@]}" claude@127.0.0.1 "sudo systemctl poweroff" >/dev/null 2>&1 || true
        wait "$boot" || true
        echo "$(date '+%T') $label done: $(/bin/cat "$out/$label.fcprobe.txt")"
        sleep 5
    done
done
mark all end
