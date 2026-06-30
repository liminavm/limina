#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# M10 disk durability probe — locate WHERE a data-disk write is lost across a guest reboot.
#
# Symptom (from tests/disks.rs first cut): an ext4 written to a second --disk (vdb) is CORRUPT
# after a guest *reboot* (blkid sees the magic, mount reports a bad superblock) — yet within one
# boot an unmount/remount round-trips fine. So the loss is specific to the worker RELAUNCH.
#
# This probe uses RAW block writes (no filesystem) to separate the layers:
#   A) after a guest `sync`, is the magic in the HOST backing file (VM still running)?
#        present  -> the write reached at least the host page cache (write() happened)
#        absent   -> it's stuck in libkrun/imago userspace cache; sync didn't flush to the file
#   B) read it back in-guest before reboot (control — must always pass)
#   C) after a guest reboot, does the guest read the magic back, and is it in the host file?
#
# Decisive: if A is ABSENT, the bug is the write/FLUSH path (not durable to the file). If A is
# PRESENT but C (guest read) is ABSENT, the new worker isn't reading the file the old one wrote.
#
# Needs HVF + network: run with the Bash tool's dangerouslyDisableSandbox. Reuses the already-
# built+codesigned target/debug binaries (run scripts/test-boot.sh once first if unsure).
set -uo pipefail
cd "$(dirname "$0")/../.."

REL="${LIMINA_FEDORA_REL:-43}"
STOCK="Fedora-Workstation-${REL}.stock.test.raw"
FW="${LIMINA_FIRMWARE:-/opt/homebrew/share/krunkit/KRUN_EFI.silent.fd}"
LIMINA="target/debug/limina"

WORK="$(mktemp -d /tmp/m10-durability.XXXXXX)"
BOOT="$WORK/boot.raw"
DATA="$WORK/data.raw"
WLOG="$WORK/worker.log"
SSH_OPTS=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes -o ConnectTimeout=10 -o LogLevel=ERROR)
MAGIC0="M10MAGIC-OFF0___"   # 16 bytes, written at byte offset 0
MAGIC1="M10MAGIC-OFF1MiB"   # 16 bytes, written at byte offset 1 MiB

echo "== setup: $WORK =="
cp -c "$STOCK" "$BOOT"                # APFS COW clone — writable boot disk (net needs rw root)
truncate -s 64M "$DATA"              # blank sparse data disk -> vdb
echo "boot=$BOOT data=$DATA"

echo "== boot limina (--net, two disks) =="
"$LIMINA" --firmware "$FW" --disk "$BOOT" --disk "$DATA" --net >"$WLOG" 2>&1 &
LIMINA_PID=$!
trap 'kill $LIMINA_PID 2>/dev/null; wait $LIMINA_PID 2>/dev/null; echo "(worker log: $WLOG)"' EXIT

# Resolve the auto-allocated SSH port from the supervisor's printed hint.
PORT=""
for _ in $(seq 1 60); do
  PORT=$(grep -oE 'ssh -p [0-9]+' "$WLOG" | head -1 | grep -oE '[0-9]+' || true)
  [ -n "$PORT" ] && break
  sleep 1
done
[ -z "$PORT" ] && { echo "FAIL: never saw the SSH-forward port in $WLOG"; tail -20 "$WLOG"; exit 1; }
echo "ssh port = $PORT"
ssh_g() { ssh "${SSH_OPTS[@]}" -p "$PORT" claude@127.0.0.1 "$@"; }

echo "== wait for sshd =="
up=""
for _ in $(seq 1 120); do
  if ssh_g true 2>/dev/null; then up=1; break; fi
  sleep 2
done
[ -z "$up" ] && { echo "FAIL: guest sshd never came up"; tail -20 "$WLOG"; exit 1; }

echo "== guest: write raw magic to /dev/vdb (off 0 + 1MiB), then sync =="
ssh_g "printf '%s' '$MAGIC0' | sudo dd of=/dev/vdb bs=1 seek=0 count=16 conv=notrunc status=none && \
       printf '%s' '$MAGIC1' | sudo dd of=/dev/vdb bs=1 seek=1048576 count=16 conv=notrunc status=none && \
       sync && echo wrote+synced" || { echo "FAIL: guest write"; exit 1; }

echo "== PROBE B (control): read magic back in-guest BEFORE reboot =="
G0=$(ssh_g "sudo dd if=/dev/vdb bs=1 skip=0 count=16 status=none")
G1=$(ssh_g "sudo dd if=/dev/vdb bs=1 skip=1048576 count=16 status=none")
echo "  guest off0   = '$G0'  (want '$MAGIC0')"
echo "  guest off1MiB= '$G1'  (want '$MAGIC1')"

echo "== PROBE A: is the magic in the HOST backing file while the VM runs? =="
H0=$(xxd -s 0       -l 16 -p "$DATA")
H1=$(xxd -s 1048576 -l 16 -p "$DATA")
echo "  host off0    = $H0  ('$(printf '%s' "$H0" | xxd -r -p)')"
echo "  host off1MiB = $H1  ('$(printf '%s' "$H1" | xxd -r -p)')"

echo "== capture pre-reboot boot id, then reboot =="
BID1=$(ssh_g "cat /proc/sys/kernel/random/boot_id")
echo "  boot id before = $BID1"
ssh_g "sudo systemd-run --on-active=1 systemctl reboot" || true

echo "== wait for a CHANGED boot id (relaunch) =="
BID2=""
for _ in $(seq 1 90); do
  cur=$(ssh_g "cat /proc/sys/kernel/random/boot_id" 2>/dev/null || true)
  if [ -n "$cur" ] && [ "$cur" != "$BID1" ]; then BID2="$cur"; break; fi
  sleep 2
done
[ -z "$BID2" ] && { echo "FAIL: guest never returned after reboot"; exit 1; }
echo "  boot id after  = $BID2 (relaunch confirmed)"

echo "== PROBE C: read magic back in-guest AFTER reboot + host file =="
RG0=$(ssh_g "sudo dd if=/dev/vdb bs=1 skip=0 count=16 status=none" 2>/dev/null || echo "<read failed>")
RG1=$(ssh_g "sudo dd if=/dev/vdb bs=1 skip=1048576 count=16 status=none" 2>/dev/null || echo "<read failed>")
RH0=$(xxd -s 0       -l 16 -p "$DATA")
RH1=$(xxd -s 1048576 -l 16 -p "$DATA")
echo "  guest off0    (post-reboot) = '$RG0'  (want '$MAGIC0')"
echo "  guest off1MiB (post-reboot) = '$RG1'  (want '$MAGIC1')"
echo "  host  off0    (post-reboot) = $RH0  ('$(printf '%s' "$RH0" | xxd -r -p)')"
echo "  host  off1MiB (post-reboot) = $RH1  ('$(printf '%s' "$RH1" | xxd -r -p)')"

echo
echo "== VERDICT =="
[ "$G0" = "$MAGIC0" ] && echo "  B (in-guest pre-reboot): PASS" || echo "  B (in-guest pre-reboot): FAIL"
if printf '%s' "$H0" | xxd -r -p | grep -q "$MAGIC0"; then
  echo "  A (host file while running): PRESENT  -> write reached the host file/page-cache"
else
  echo "  A (host file while running): ABSENT   -> write stuck in libkrun/imago userspace cache"
fi
[ "$RG0" = "$MAGIC0" ] && echo "  C (in-guest post-reboot): PASS — data survived the reboot" \
                       || echo "  C (in-guest post-reboot): FAIL — data LOST across the reboot"
