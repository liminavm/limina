#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# Can a stock Fedora guest's qemu-guest-agent set the memory-hotplug onlining knobs?
#
# Boots a CoW clone of the stock test image (Enforcing, the image's own kernel), starts a second
# qemu-ga from systemd on a unix socket so it runs in the agent's real domain, and has it write
# each knob two ways: guest-file-open/write on the sysfs file, and guest-exec of `sh -c 'echo >'`.
# It also has it create files in /etc/modprobe.d, /etc/udev/rules.d and /etc/tmpfiles.d, the
# places a write-once guest hook would live. dontaudit rules are off for the run, so a silent
# denial shows. Results: RESULTS.md.
#
#   spikes/qga-sysfs-movable/probe.sh [image.raw]
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO"

SRC="${1:-$REPO/Fedora-Workstation-44.stock.test.raw}"
SCRATCH="${SCRATCH:-${TMPDIR:-/tmp}}/qga-sysfs-probe-$$"
DISK="$SCRATCH/qga-sysfs-probe.raw"
LOG="$SCRATCH/worker.log"
SSH_OPTS=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR)

mkdir -p "$SCRATCH"
BOOT=""
cleanup() {
  if [ -n "$BOOT" ] && kill -0 "$BOOT" 2>/dev/null; then
    kill "$BOOT" 2>/dev/null || true
    wait "$BOOT" 2>/dev/null || true
  fi
  rm -rf "$SCRATCH"
}
trap cleanup EXIT

cp -c "$SRC" "$DISK"
LIMINA_DISK="$DISK" LIMINA_BOOT_LOG="$LOG" spikes/venus-draw-probe/boot-enhanced-efi-kk.sh \
  >"$SCRATCH/boot.log" 2>&1 &
BOOT=$!
PORT="$(scripts/wait-guest-ssh.sh "$LOG" 300 "$BOOT")"
g() { ssh -p "$PORT" "${SSH_OPTS[@]}" claude@127.0.0.1 "$@"; }
scp -P "$PORT" "${SSH_OPTS[@]}" spikes/qga-sysfs-movable/qga_probe.py claude@127.0.0.1:/tmp/ >/dev/null

g 'sudo bash -s' <<'EOF'
set -u
echo "== guest"
uname -r; getenforce; rpm -q qemu-guest-agent selinux-policy-targeted
echo "== the live agent"
ps -eZ | grep '[q]emu-ga'
systemctl is-active qemu-guest-agent
grep -E '^[^#]*(BLOCK_RPCS|ALLOW_RPCS|FSFREEZE)' /etc/sysconfig/qemu-ga 2>/dev/null || echo "(no rpc filter in /etc/sysconfig/qemu-ga)"
echo "== labels"
ls -Z /sys/devices/system/memory/auto_online_blocks \
      /sys/module/memory_hotplug/parameters/online_policy \
      /sys/module/memory_hotplug/parameters/auto_movable_ratio
ls -dZ /etc/modprobe.d /etc/udev/rules.d /etc/tmpfiles.d
echo "== booleans"
getsebool -a | grep -iE 'qemu_ga|virt_qemu'
semodule -DB

# Its own state dir, labelled like the live agent's /run files, so the two never share state.
mkdir -p /run/qga-probe && chcon -t virt_qemu_ga_var_run_t /run/qga-probe
# The agent's domain may not create a socket file, so systemd creates it and hands it over
# (qemu-ga takes a socket-activated listener in place of -p).
systemd-run --unit=qga-probe --quiet --socket-property=ListenStream=/run/qga-probe/qga.sock \
  /usr/bin/qemu-ga -m unix-listen -t /run/qga-probe -f /run/qga-probe/qga.pid
# A socket-activated service starts on the first connection.
python3 -c 'import socket; s=socket.socket(socket.AF_UNIX); s.connect("/run/qga-probe/qga.sock")'
sleep 2
pid="$(systemctl show -P MainPID qga-probe)"
echo "== the probe agent: pid $pid, $(systemctl is-active qga-probe)"
cat "/proc/$pid/attr/current"; echo
ls -Z /run/qga-probe/qga.sock 2>&1
journalctl -q -u qga-probe --no-pager
ausearch --input-logs -m avc -c qemu-ga -ts recent 2>/dev/null | grep -o 'avc: .*' || true

echo "== probe"
python3 /tmp/qga_probe.py /run/qga-probe/qga.sock </dev/null

echo "== static policy"
dnf -y -q install setools-console >/dev/null 2>&1 || echo "(setools-console unavailable)"
for t in sysfs_t modules_conf_t udev_rules_t etc_t; do
  echo "-- allow virt_qemu_ga_t -> $t file"
  sesearch -A -s virt_qemu_ga_t -t "$t" -c file 2>&1
  sesearch -A -s virt_qemu_ga_t -t "$t" -c dir 2>&1
done
echo "-- dontaudit virt_qemu_ga_t -> sysfs_t"
sesearch --dontaudit -s virt_qemu_ga_t -t sysfs_t 2>&1
echo "-- every file type virt_qemu_ga_t may write"
sesearch -A -s virt_qemu_ga_t -c file -p write 2>&1 | grep 'allow virt_qemu_ga_t'
semodule -B
systemctl stop qga-probe.socket qga-probe
EOF

g 'sudo systemctl poweroff -i' >/dev/null 2>&1 || true
wait "$BOOT" 2>/dev/null || true
BOOT=""
