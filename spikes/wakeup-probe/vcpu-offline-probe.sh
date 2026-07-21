#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# vcpu-offline-probe.sh — de-risk spike for task #35 (dynamic vCPU offlining).
#
# THE two questions this answers empirically (source review found libkrun's PSCI models
# CPU_ON but NOT CPU_OFF — it returns NOT_SUPPORTED — and CPU_ON is delivered via a
# ONE-SHOT boot_receiver.recv() consumed at boot):
#   1. When the guest offlines a running vCPU (echo 0 > cpuN/online), does the host vCPU
#      thread PARK (worker %CPU + wakeups drop → the win) or SPIN (worker %CPU jumps by
#      ~1 core per offlined vCPU → worse than leaving it online)?
#   2. Does re-onlining (echo 1) bring the vCPU back, or hang (one-shot CPU_ON channel)?
#
# Read-only w.r.t. the pristine image: boots a throwaway APFS clone. Headless (no display →
# no KK/venus dep). Needs a signed worker + the ≥7.1 injected kernel + F43 stock.test.
set -euo pipefail
cd "$(dirname "$0")/../.."

KERNEL="${KERNEL:-target/test-guest/kernel/Image-16k-71}"
SRC_DISK="${SRC_DISK:-Fedora-Workstation-43.stock.test.raw}"
CLONE="${CLONE:-spike-vcpu-clone.raw}"
CPUS="${CPUS:-6}"
CMDLINE="root=/dev/vda3 rootflags=subvol=root rootfstype=btrfs rw selinux=0 console=ttyAMA0"
SUPLOG="${SUPLOG:-$PWD/spike-vcpu-sup.log}"
WORKER="target/debug/limina-vmm"

ssh_g() { ssh -p "$PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
          -o BatchMode=yes -o ConnectTimeout=10 -o LogLevel=ERROR "claude@127.0.0.1" "$@"; }

# Host worker instantaneous %CPU (2nd top sample) — the park-vs-spin discriminator.
worker_cpu() { /usr/bin/top -l 2 -pid "$1" -stats cpu 2>/dev/null | grep -E '^[0-9]' | tail -1; }

echo "==> cloning $SRC_DISK -> $CLONE (APFS)"
rm -f "$CLONE"; cp -c "$SRC_DISK" "$CLONE"

echo "==> booting: $CPUS vCPUs, kernel $KERNEL, headless + NAT"
rm -f "$SUPLOG"
RUST_LOG="${RUST_LOG:-warn,limina=info}" target/debug/limina \
    --kernel "$KERNEL" --disk "$CLONE" --cmdline "$CMDLINE" \
    --cpus "$CPUS" --ram-mib 4096 --net > "$SUPLOG" 2>&1 &
SUP_PID=$!
trap 'echo "==> teardown"; kill "$SUP_PID" 2>/dev/null || true; sleep 2; pkill -f "$WORKER" 2>/dev/null || true; rm -f "$CLONE"' EXIT

echo "==> waiting for the SSH forward port from the supervisor log"
for _ in $(seq 1 60); do
    PORT=$(grep -oE 'ssh -p [0-9]+' "$SUPLOG" | head -1 | grep -oE '[0-9]+') && [ -n "${PORT:-}" ] && break
    sleep 1
done
[ -n "${PORT:-}" ] || { echo "no ssh port"; cat "$SUPLOG"; exit 1; }
echo "    ssh port $PORT"

echo "==> waiting for sshd"
for _ in $(seq 1 90); do ssh_g true 2>/dev/null && break; sleep 2; done
ssh_g true || { echo "sshd never came up"; tail -30 "$SUPLOG"; exit 1; }

WPID=$(pgrep -f "$WORKER" | head -1)
echo "    worker pid $WPID; guest kernel $(ssh_g uname -r); nproc $(ssh_g nproc)"

echo "===================== BASELINE ($CPUS vCPUs online) ====================="
echo "-- guest online cpus: $(ssh_g 'cat /sys/devices/system/cpu/online')"
echo "-- worker %CPU (idle): $(worker_cpu "$WPID")"
spikes/wakeup-probe/procwake "$WPID" 3 3 2>/dev/null | tail -4 || true
BASE_INT=$(ssh_g 'cat /proc/interrupts' | grep -E 'IPI0|arch_timer|Rescheduling|Function call' || true)

echo "===================== OFFLINE cpus 2..$((CPUS-1)) (keep 0,1) ====================="
for i in $(seq 2 $((CPUS-1))); do
    ssh_g "echo 0 | sudo tee /sys/devices/system/cpu/cpu$i/online >/dev/null" 2>&1 || echo "offline cpu$i FAILED"
done
sleep 3
echo "-- guest online cpus NOW: $(ssh_g 'cat /sys/devices/system/cpu/online')"
echo "-- guest nproc NOW: $(ssh_g nproc)"
echo "-- worker %CPU after offline (SPIN if ~+400%, PARK if flat/low): $(worker_cpu "$WPID")"
spikes/wakeup-probe/procwake "$WPID" 3 3 2>/dev/null | tail -4 || true
echo "-- PSCI CPU_OFF handling in the worker log (NOT_SUPPORTED = unmodeled):"
grep -iE 'unhandled PSCI|PSCI.*0x8400_0002|CPU_OFF' "$SUPLOG" | tail -5 || echo "   (no PSCI warn logged)"

# macOS has no `timeout`; wrap each re-online ssh in a background+watchdog-kill so a hung
# re-online (the one-shot CPU_ON boot channel means this is EXPECTED on unpatched libkrun)
# fails the step instead of blocking the probe.
ssh_g_timed() { # $1 = seconds, rest = remote cmd
    local secs="$1"; shift
    ssh -p "$PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
        -o BatchMode=yes -o "ConnectTimeout=$secs" -o LogLevel=ERROR "claude@127.0.0.1" "$@" &
    local pid=$!
    ( sleep "$secs"; kill "$pid" 2>/dev/null ) & local watch=$!
    if wait "$pid" 2>/dev/null; then kill "$watch" 2>/dev/null; return 0; else return 1; fi
}
echo "===================== RE-ONLINE cpus 2..$((CPUS-1)) ====================="
for i in $(seq 2 $((CPUS-1))); do
    ssh_g_timed 15 "echo 1 | sudo tee /sys/devices/system/cpu/cpu$i/online >/dev/null" \
        && echo "   re-online cpu$i: OK" || echo "   re-online cpu$i: FAILED/HUNG (~15s)"
done
sleep 2
echo "-- guest online cpus after re-online: $(ssh_g 'cat /sys/devices/system/cpu/online' 2>/dev/null || echo '(ssh hung)')"
echo "-- guest nproc after re-online: $(ssh_g nproc 2>/dev/null || echo '(ssh hung)')"
echo "-- worker %CPU after re-online: $(worker_cpu "$WPID")"

echo "==> DONE"
