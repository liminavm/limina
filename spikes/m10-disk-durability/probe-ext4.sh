#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# M10 disk durability probe #2 — localize the EXT4-after-reboot corruption.
#
# probe.sh proved RAW block writes (with sync) survive a reboot and vdb stays vdb. Yet an ext4
# made on vdb came up corrupt after a reboot (tests/disks.rs first cut: blkid sees the magic,
# mount says bad superblock). This narrows it to ext4's larger/scattered metadata writes.
#
# Strategy: checksum the first 8 MiB of vdb (where ext4's superblock + group descriptors + inode
# tables + journal live) from BOTH the guest and the host backing file, BEFORE and AFTER a
# reboot. The four sums pinpoint the divergence:
#   guest-pre == host-pre   : the guest's mkfs/sync reached the host file (write path OK)
#   guest-post != guest-pre : the new worker serves different bytes than were written
#   host-pre  != host-post  : the host file itself changed across the relaunch
# Plus: does `mount` succeed after reboot, and what does dmesg say.
#
# Needs HVF + network (dangerouslyDisableSandbox). Reuses target/debug binaries.
set -uo pipefail
cd "$(dirname "$0")/../.."

REL="${LIMINA_FEDORA_REL:-43}"
STOCK="Fedora-Workstation-${REL}.stock.test.raw"
FW="${LIMINA_FIRMWARE:-/opt/homebrew/share/krunkit/KRUN_EFI.silent.fd}"
LIMINA="target/debug/limina"

WORK="$(mktemp -d /tmp/m10-ext4.XXXXXX)"
BOOT="$WORK/boot.raw"; DATA="$WORK/data.raw"; WLOG="$WORK/worker.log"
SSH_OPTS=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes -o ConnectTimeout=10 -o LogLevel=ERROR)
SPAN=$((8 * 1024 * 1024))   # first 8 MiB

echo "== setup: $WORK =="
cp -c "$STOCK" "$BOOT"
truncate -s 256M "$DATA"
host_sum() { dd if="$DATA" bs=1M count=8 status=none 2>/dev/null | shasum -a 256 | cut -d' ' -f1; }

echo "== boot limina (--net, two disks) =="
"$LIMINA" --firmware "$FW" --disk "$BOOT" --disk "$DATA" --net >"$WLOG" 2>&1 &
LIMINA_PID=$!
trap 'kill $LIMINA_PID 2>/dev/null; wait $LIMINA_PID 2>/dev/null; echo "(worker log: $WLOG)"' EXIT

PORT=""
for _ in $(seq 1 60); do
  PORT=$(grep -oE 'ssh -p [0-9]+' "$WLOG" | head -1 | grep -oE '[0-9]+' || true); [ -n "$PORT" ] && break; sleep 1
done
[ -z "$PORT" ] && { echo "FAIL: no SSH port"; tail -20 "$WLOG"; exit 1; }
echo "ssh port = $PORT"
ssh_g() { ssh "${SSH_OPTS[@]}" -p "$PORT" claude@127.0.0.1 "$@"; }

echo "== wait for sshd =="
up=""; for _ in $(seq 1 120); do ssh_g true 2>/dev/null && { up=1; break; }; sleep 2; done
[ -z "$up" ] && { echo "FAIL: no sshd"; tail -20 "$WLOG"; exit 1; }

echo "== guest: mkfs.ext4 + mount + write + sync + umount (the failing sequence) =="
ssh_g "sudo mkfs.ext4 -q -F /dev/vdb && sudo mkdir -p /mnt/data && sudo mount /dev/vdb /mnt/data && \
       echo m10-ext4-marker | sudo tee /mnt/data/marker >/dev/null && sync && sudo umount /mnt/data && echo done" \
  || { echo "FAIL: mkfs/mount/write"; exit 1; }

echo "== checksums BEFORE reboot (first 8 MiB) =="
GPRE=$(ssh_g "sudo dd if=/dev/vdb bs=1M count=8 status=none | sha256sum | cut -d' ' -f1")
HPRE=$(host_sum)
echo "  guest-pre = $GPRE"
echo "  host-pre  = $HPRE"
BPRE=$(ssh_g "sudo blkid -o value -s TYPE /dev/vdb")
echo "  blkid pre = $BPRE"

echo "== reboot =="
BID1=$(ssh_g "cat /proc/sys/kernel/random/boot_id")
ssh_g "sudo systemd-run --on-active=1 systemctl reboot" || true
BID2=""; for _ in $(seq 1 90); do c=$(ssh_g "cat /proc/sys/kernel/random/boot_id" 2>/dev/null||true); [ -n "$c" ] && [ "$c" != "$BID1" ] && { BID2=$c; break; }; sleep 2; done
[ -z "$BID2" ] && { echo "FAIL: no return after reboot"; exit 1; }
echo "  relaunch confirmed ($BID1 -> $BID2)"

echo "== checksums AFTER reboot =="
ssh_g "test -b /dev/vdb" || { echo "FAIL: vdb gone"; exit 1; }
GPOST=$(ssh_g "sudo dd if=/dev/vdb bs=1M count=8 status=none | sha256sum | cut -d' ' -f1")
HPOST=$(host_sum)
BPOST=$(ssh_g "sudo blkid -o value -s TYPE /dev/vdb" 2>/dev/null || echo "<none>")
echo "  guest-post = $GPOST"
echo "  host-post  = $HPOST"
echo "  blkid post = $BPOST"

echo "== mount after reboot? =="
MOUNT=$(ssh_g "sudo mount /dev/vdb /mnt/data 2>&1 && cat /mnt/data/marker && sudo umount /mnt/data" 2>&1 || echo "MOUNT-FAILED: $(ssh_g 'sudo dmesg | tail -5' 2>/dev/null)")
echo "  $MOUNT"

echo
echo "== VERDICT =="
echo "  blkid:  pre=$BPRE  post=$BPOST"
[ "$GPRE" = "$HPRE" ]  && echo "  write path: guest-pre == host-pre  (mkfs/sync reached the host file)" \
                       || echo "  write path: guest-pre != host-pre  (host file MISSING the guest's writes!)"
[ "$GPRE" = "$GPOST" ] && echo "  guest view: UNCHANGED across reboot (data intact to the guest)" \
                       || echo "  guest view: CHANGED across reboot (new worker serves different bytes)"
[ "$HPRE" = "$HPOST" ] && echo "  host file:  UNCHANGED across reboot" \
                       || echo "  host file:  CHANGED across reboot (relaunch rewrote/clobbered it)"
echo "$MOUNT" | grep -q m10-ext4-marker && echo "  ext4 remount: OK" || echo "  ext4 remount: FAILED (corruption reproduced)"
