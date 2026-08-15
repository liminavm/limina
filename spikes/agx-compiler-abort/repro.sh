#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# Reproduce the AGX Metal compiler `bitcode_url` abort (task #29) and report, in one line,
# whether the worker died. Boots a clone of the synoik enhanced image, waits for the guest,
# starts the reporter's workload mix, and watches until either the worker aborts or the
# timeout expires.
#
# The workload is the user's original recipe -- vkcube + vkmark + glmark2-wayland + Firefox on
# https://web.gpuscore.com/run. Note the benchmark does NOT need to be started: the first
# reproduction aborted while the page was merely sitting on its Start button, so the trigger is
# in the ordinary compositor/venus traffic, not in the benchmark's own draws.
#
# Usage: spikes/agx-compiler-abort/repro.sh [label] [timeout_seconds]
#   Env passed through to the worker is the A/B lever, e.g.
#     LIMINA_KK_MTLTEXTURE_SCANOUT=0 spikes/agx-compiler-abort/repro.sh scanout-off
#
# Exit status: 0 = worker aborted (reproduced), 1 = survived the timeout (did not reproduce).
set -uo pipefail

cd "$(dirname "$0")/../.." || exit 2
LABEL="${1:-run}"
TIMEOUT="${2:-240}"
SRC=Fedora-Workstation-44.enhanced.synoik.raw
DISK="agx-repro.raw"
OUT="/tmp/agx-repro-${LABEL}"
mkdir -p "$OUT"

# APFS clonefile: instant, and it keeps the pristine image pristine across runs. Each run starts
# from the same disk state, which matters -- a dirty guest is a different system under test.
rm -f "$DISK"; cp -c "$SRC" "$DISK" || exit 2

nohup cargo xtask run --disk "$DISK" > "$OUT/vm.log" 2>&1 &
echo "[$LABEL] booting (LIMINA_KK_MTLTEXTURE_SCANOUT=${LIMINA_KK_MTLTEXTURE_SCANOUT:-default})" >&2

deadline=$(( $(date +%s) + TIMEOUT ))

# Other sessions run VMs on this host, so NOTHING here may match on a bare process name: a
# global `pgrep -x limina-vmm` would see their worker and a `pkill` would take down their
# supervisor. Scope everything to the supervisor pid this run printed, and take the worker as
# its child. Same reason the shared /tmp/enhanced-efi-kk-worker.log is only consulted for the
# ssh port and only via this run's own log line -- a concurrent boot overwrites that file.
sup=""
while [ -z "$sup" ] && [ "$(date +%s)" -lt "$deadline" ]; do
  sup=$(grep -oE '^limina pid=[0-9]+' "$OUT/vm.log" 2>/dev/null | tail -1 | cut -d= -f2)
  [ -z "$sup" ] && sleep 2
done
[ -z "$sup" ] && { echo "[$LABEL] never got a supervisor pid" >&2; exit 2; }
echo "[$LABEL] supervisor pid=$sup" >&2

port=""
while [ -z "$port" ] && [ "$(date +%s)" -lt "$deadline" ]; do
  port=$(grep -oE 'ssh -p [0-9]+' /tmp/enhanced-efi-kk-worker.log 2>/dev/null | tail -1 | awk '{print $3}')
  [ -z "$port" ] && sleep 2
done
[ -z "$port" ] && { echo "[$LABEL] never got an ssh port" >&2; exit 2; }

while ! ssh -o StrictHostKeyChecking=no -o ConnectTimeout=3 -p "$port" claude@127.0.0.1 true 2>/dev/null; do
  [ "$(date +%s)" -ge "$deadline" ] && { echo "[$LABEL] guest never came up" >&2; exit 2; }
  sleep 5
done
echo "[$LABEL] guest up on port $port; starting load" >&2

# WORKLOAD selects which of the reporter's four apps to start, so the trigger can be narrowed by
# leaving one out at a time -- the abort is deterministic at 79s, which makes that cheap.
WORKLOAD="${WORKLOAD:-vkcube,vkmark,glmark2,firefox}"
# VKMARK_ARGS narrows to a single vkmark scene once one is implicated.
VKMARK_ARGS="${VKMARK_ARGS:-}"
echo "[$LABEL] workload: $WORKLOAD" >&2

ssh -p "$port" claude@127.0.0.1 "WORKLOAD='$WORKLOAD' VKMARK_ARGS='$VKMARK_ARGS' bash -s" <<'GUEST'
export XDG_RUNTIME_DIR=/run/user/1000
export WAYLAND_DISPLAY=$(basename /run/user/1000/wayland-[0-9] | head -1)
export GALLIUM_DRIVER=virgl MESA_LOADER_DRIVER_OVERRIDE=virtio_gpu
export VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json
export MOZ_ENABLE_WAYLAND=1
cd /tmp
case ",$WORKLOAD," in *,vkcube,*)  nohup vkcube          > /tmp/vkcube.log  2>&1 & ;; esac
case ",$WORKLOAD," in *,vkmark,*)  nohup vkmark $VKMARK_ARGS > /tmp/vkmark.log 2>&1 & ;; esac
case ",$WORKLOAD," in *,glmark2,*) nohup glmark2-wayland > /tmp/glmark2.log 2>&1 & ;; esac
sleep 3
case ",$WORKLOAD," in *,firefox,*)
  nohup firefox --new-window "https://web.gpuscore.com/run" > /tmp/firefox.log 2>&1 & ;;
esac
echo "guest load started: $WORKLOAD"
GUEST

start=$(date +%s)
while [ "$(date +%s)" -lt "$deadline" ]; do
  if ! kill -0 "$sup" 2>/dev/null; then
    took=$(( $(date +%s) - start ))
    # Snapshot the worker log: /tmp/enhanced-efi-kk-worker.log is a fixed path that the NEXT run
    # overwrites, and with LIMINA_KK_RPLOG on it is the whole point of the run.
    cp /tmp/enhanced-efi-kk-worker.log "$OUT/worker.log" 2>/dev/null
    # signal 6 is the abort; anything else is some other death and must not be scored as a repro.
    if grep -q "terminated by signal 6" "$OUT/vm.log" "$OUT/worker.log" 2>/dev/null; then
      echo "[$LABEL] REPRODUCED: worker aborted ${took}s after load start"
      exit 0
    fi
    echo "[$LABEL] supervisor $sup exited WITHOUT signal 6 after ${took}s -- not this bug" >&2
    exit 1
  fi
  sleep 3
done

echo "[$LABEL] survived ${TIMEOUT}s -- did not reproduce"
cp /tmp/enhanced-efi-kk-worker.log "$OUT/worker.log" 2>/dev/null
kill "$sup" 2>/dev/null   # only ever this run's supervisor -- never a name match
exit 1
