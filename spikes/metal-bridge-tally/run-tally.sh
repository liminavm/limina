#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Boot an enhanced clone on the instrumented KK with both tallies armed, run each workload for a
# fixed time, and keep a host-timestamped mirror of the worker log so analyze.py can assign every
# report window to the workload that produced it.
#
# Usage: run-tally.sh <disk clone> <out dir> [workload...]
#   The clone boots IN PLACE: make it with `cp -c Fedora-Workstation-44.enhanced.raw <clone>`.
#   Workloads (default: all, in this order): idle vkmark vk-replay drawstorm gl-replay-venus
#   gl-replay-vrend glmark2-venus glmark2-vrend
# Env: DURATION (s per workload, default 30), TALLY_EVERY (s, default 5),
#      KK_ICD (default: the build-kk-tally devenv json), LIMINA_CPUS / LIMINA_RAM_MIB (4 / 4096).
set -euo pipefail
cd "$(dirname "$0")/../.."
ROOT="$(pwd)"
SPIKE="$ROOT/spikes/metal-bridge-tally"

DISK="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
OUT="$2"; shift 2
WORKLOADS=("$@")
[ ${#WORKLOADS[@]} -gt 0 ] || WORKLOADS=(idle vkmark vk-replay drawstorm gl-replay-venus gl-replay-vrend glmark2-venus glmark2-vrend)
DURATION="${DURATION:-30}"
EVERY="${TALLY_EVERY:-5}"
KK_ICD="${KK_ICD:-/Volumes/mesa-cs/build-kk-tally/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.aarch64.json}"
[ -f "$DISK" ] || { echo "no disk $DISK" >&2; exit 1; }
case "$(basename "$DISK")" in Fedora-Workstation-44.enhanced*.raw)
  echo "refusing to boot a canonical image in place; clone it with cp -c" >&2; exit 1;; esac
mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"

WLOG="$OUT/worker.log"
STAMPED="$OUT/worker.stamped.log"
MARKS="$OUT/marks.tsv"
: >"$MARKS"

# Debug channels open per the poke-VM rule; the two tallies are what this run is for.
export RUST_LOG="warn,limina=info,krun::vmm=info,krun_devices=info"
export LIMINA_POINTER_WIRE_TRACE=1
export LIMINA_WINDOW_CAPTURE="$OUT/window.png"
export LIMINA_KK_BRIDGE_TALLY="$EVERY" VIRGLRS_SUBMIT_STATS="$EVERY"
export LIMINA_KK_ICD="$KK_ICD" LIMINA_BOOT_LOG="$WLOG" LIMINA_DISK="$DISK"
export LIMINA_CPUS="${LIMINA_CPUS:-4}" LIMINA_RAM_MIB="${LIMINA_RAM_MIB:-4096}"
# A pinned port, away from the auto-allocated 2222+ range other sessions' VMs come and go on.
# An auto port is reused by the next VM to boot once ours dies, so a script that keeps sshing to
# "its" port can land on -- and power off -- somebody else's guest. It has happened both ways.
SSH_PORT="${SSH_PORT:-2291}"
if lsof -nP -iTCP:"$SSH_PORT" -sTCP:LISTEN >/dev/null 2>&1; then
  echo "port $SSH_PORT is already taken; set SSH_PORT" >&2; exit 1
fi
export LIMINA_EXTRA_ARGS="--ssh-port $SSH_PORT${LIMINA_EXTRA_ARGS:+ $LIMINA_EXTRA_ARGS}"

# The boot vehicle ends in `wait` on the supervisor, so it runs in the background and names the
# supervisor's pid on its first line.
spikes/venus-draw-probe/boot-enhanced-efi-kk.sh >"$OUT/boot.out" 2>&1 &
for _ in $(seq 60); do grep -q '^limina pid=' "$OUT/boot.out" && break; sleep 0.5; done
cat "$OUT/boot.out"
BOOT_PID="$(sed -n 's/^limina pid=\([0-9]*\).*/\1/p' "$OUT/boot.out")"
[ -n "$BOOT_PID" ] || { echo "the boot vehicle never named the supervisor pid" >&2; exit 1; }
echo "worker log: $WLOG   (tail -f $WLOG)"

# The mirror: every worker-log line prefixed with host epoch seconds as it lands.
( tail -n +1 -F "$WLOG" 2>/dev/null | perl -MTime::HiRes=time -ne '$|=1; printf "%.3f %s", time, $_' ) \
  >"$STAMPED" &
MIRROR=$!
trap 'kill $MIRROR 2>/dev/null || true' EXIT

PORT="$(scripts/wait-guest-ssh.sh "$WLOG" 300 "$BOOT_PID")"
[ "$PORT" = "$SSH_PORT" ] || { echo "guest came up on $PORT, not the pinned $SSH_PORT" >&2; exit 1; }
SSH0=(ssh -p "$PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR
      -o BatchMode=yes claude@127.0.0.1)
# Identity token in the guest's tmpfs: every later command runs only where it matches, so even a
# port mix-up cannot run a workload on -- or power off -- another session's guest.
TOKEN="mnc-$$-$RANDOM$RANDOM"
"${SSH0[@]}" "echo $TOKEN > /run/user/1000/mnc-token"
SSH=("${SSH0[@]}" "[ \"\$(cat /run/user/1000/mnc-token 2>/dev/null)\" = $TOKEN ] || { echo NOT-OUR-GUEST >&2; exit 97; };")
SCP=(scp -P "$PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR)
MAIN="$(git -C "$ROOT" worktree list --porcelain | awk 'NR==1{print $2}')"
# The guest image has no glslang; the SPIR-V is target-independent, so compile it here.
glslangValidator -V spikes/venus-cmdstream-probe/shader.vert -o "$OUT/vert.spv" >/dev/null
glslangValidator -V spikes/venus-cmdstream-probe/shader.frag -o "$OUT/frag.spv" >/dev/null
"${SCP[@]}" "$MAIN/fixtures/traces/glmark2-build.trace" "$MAIN/fixtures/traces/vkcube.gfxr" \
  spikes/venus-cmdstream-probe/drawstorm.c "$OUT/vert.spv" "$OUT/frag.spv" \
  "$SPIKE/guest-workload.sh" claude@127.0.0.1:/tmp/

# Let the session settle (gnome-shell start-up compiles and allocates) before the first window.
sleep 30
"${SSH[@]}" "bash /tmp/guest-workload.sh provenance" >"$OUT/provenance.txt" 2>&1 || true

for w in "${WORKLOADS[@]}"; do
  kill -0 "$BOOT_PID" 2>/dev/null || { echo "!! the VM died before $w; stopping" >&2; break; }
  echo "==> $w (${DURATION}s)"
  t0="$(perl -MTime::HiRes=time -e 'printf "%.3f", time')"
  "${SSH[@]}" "bash /tmp/guest-workload.sh $w $DURATION" >"$OUT/$w.txt" 2>&1 || echo "    $w exited nonzero (see $OUT/$w.txt)"
  t1="$(perl -MTime::HiRes=time -e 'printf "%.3f", time')"
  printf '%s\t%s\t%s\n' "$w" "$t0" "$t1" >>"$MARKS"
  # A quiet gap so no window straddles two workloads' steady states.
  sleep "$((EVERY * 2))"
done

if kill -0 "$BOOT_PID" 2>/dev/null; then
  "${SSH[@]}" "sudo systemctl poweroff" || true
  for _ in $(seq 60); do kill -0 "$BOOT_PID" 2>/dev/null || break; sleep 1; done
else
  echo "!! the VM was already gone before the poweroff (supervisor $BOOT_PID exited)" >&2
fi
sleep 2
echo "==> done: $OUT  (python3 $SPIKE/analyze.py $OUT)"
