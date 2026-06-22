#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# RED/GREEN oracle for the windowed-GOP firmware-BDS hang (and its fix).
#
# The hang wedges EDK2 *before* the kernel starts, so "did we get past firmware" == "did the
# guest's kernel boot, bring up NAT, and answer SSH on 2222". We launch the windowed GOP boot
# (NO --console, NO forced firmware — the exact config that hung 5/5) and poll SSH:
#   BOOT  -> SSH answered within the timeout  (past firmware, kernel up)
#   HANG  -> no SSH before the timeout        (wedged in firmware BDS)
# Run it N times to measure determinism. Usage: firmware-hang-probe.sh [N] [timeout_secs]
set -uo pipefail
cd "$(dirname "$0")/../.."
REPO="$PWD"

N="${1:-1}"
TIMEOUT="${2:-110}"
SSH_OPTS=(-p 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null
          -o ConnectTimeout=4 -o BatchMode=yes claude@127.0.0.1)

cleanup() {
  pkill -KILL -f "target/debug/limina " 2>/dev/null || true
  pkill -KILL -f "target/debug/limina-vmm" 2>/dev/null || true
  pkill -KILL -f "gvproxy.*limina-gvproxy" 2>/dev/null || true
  # wait for port 2222 to be released
  for _ in $(seq 1 20); do
    lsof -nP -iTCP:2222 -sTCP:LISTEN >/dev/null 2>&1 || break
    sleep 0.25
  done
}

boot=0; hang=0
for i in $(seq 1 "$N"); do
  echo "===== attempt $i/$N ====="
  cleanup
  log="/tmp/firmware-hang-probe.$i.log"
  RUST_LOG=info spikes/virgl-zink-kk/boot-virgl-windowed.sh >"$log" 2>&1 &
  launch_pid=$!
  echo "  launched (pid $launch_pid), log $log; polling SSH for ${TIMEOUT}s ..."

  result="HANG"
  for _ in $(seq 1 "$TIMEOUT"); do
    if ! kill -0 "$launch_pid" 2>/dev/null; then
      # the launcher exited on its own — boot failed/crashed, not a hang
      result="EXITED"; break
    fi
    if ssh "${SSH_OPTS[@]}" 'true' 2>/dev/null; then
      result="BOOT"; break
    fi
    sleep 1
  done

  if [ "$result" = "BOOT" ]; then
    boot=$((boot+1))
    echo "  BOOT — $(ssh "${SSH_OPTS[@]}" 'uname -r; uptime' 2>/dev/null | tr '\n' ' ')"
    ssh "${SSH_OPTS[@]}" 'sudo systemctl poweroff' 2>/dev/null || true
  else
    hang=$((hang+1))
    echo "  $result — no SSH within ${TIMEOUT}s; last serial/worker tail:"
    tail -5 "$log" 2>/dev/null | sed 's/^/    /'
  fi
  cleanup
done

echo
echo "===== RESULT: BOOT=$boot  HANG/EXITED=$hang  (of $N) ====="
