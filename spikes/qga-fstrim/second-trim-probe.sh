#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# Does a SECOND guest trim return the host blocks the first one left behind?
#
# crates/limina-test/tests/l2_qga_fstrim.rs asserts that one trim returns more than half of a
# 2048 MiB payload, and it sits right on that line: four measured runs recovered 1012-1074 MiB,
# so the same unchanged code passes or fails on a few MiB of noise. This probe asks whether the
# residue is permanent or merely not-yet-trimmed, by running the trim repeatedly and reading the
# host file's allocation after each one.
#
# Vehicle matches the test's: the enhanced test image, root remounted `nodiscard` so Fedora's own
# discard=async cannot return anything on its own, a 2048 MiB incompressible payload written and
# deleted. The trim is `fstrim /` over ssh rather than the supervisor's qga tick, because both
# reach the same FITRIM -> virtio-blk DISCARD -> imago punch-hole path and this one is promptable.
#
# Host allocation is st_blocks on the backing file, never the guest's own `fstrim -v` number:
# that reports the size of the ranges walked, which was 25.7 GiB in a run that recovered 958 MiB.
#
#   spikes/qga-fstrim/second-trim-probe.sh            # 1M, 1M, 0, 0
#   MINIMUMS="0 1M" spikes/qga-fstrim/second-trim-probe.sh
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO"

PAYLOAD_MIB="${PAYLOAD_MIB:-2048}"
SRC="${SRC:-$REPO/Fedora-Workstation-44.enhanced.test.raw}"
SCRATCH="${SCRATCH:-${TMPDIR:-/tmp}}/fstrim-probe-$$"
DISK="$SCRATCH/disk.raw"
BOOT_LOG="$SCRATCH/boot.log"
SSH_OPTS=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes)

[ -f "$SRC" ] || { echo "no source image at $SRC" >&2; exit 1; }
mkdir -p "$SCRATCH"

BOOT_PID=""
cleanup() {
    if [ -n "$BOOT_PID" ] && kill -0 "$BOOT_PID" 2>/dev/null; then
        kill "$BOOT_PID" 2>/dev/null || true
        # Give the supervisor a moment to take the worker down with it.
        for _ in $(seq 1 20); do kill -0 "$BOOT_PID" 2>/dev/null || break; sleep 1; done
    fi
    rm -rf "$SCRATCH"
}
trap cleanup EXIT

# Allocated size of the backing file, in MiB. st_blocks (512 B units), matching the test's
# `allocated_mib`, so the numbers here are directly comparable to the ones it prints.
allocated_mib() { echo $(( $(stat -f %b "$DISK") / 2048 )); }

g() { ssh -p "$PORT" "${SSH_OPTS[@]}" claude@127.0.0.1 "$@"; }

echo "==> cloning $(basename "$SRC") (APFS CoW, free)"
cp -c "$SRC" "$DISK"

echo "==> booting"
target/debug/limina \
    --firmware target/krun-efi/KRUN_EFI.gop.fd \
    --disk "$DISK" --net \
    > "$BOOT_LOG" 2>&1 &
BOOT_PID=$!
# The supervisor prints `guest SSH forward ready: ...` on its own stdout, which is $BOOT_LOG.
PORT="$(scripts/wait-guest-ssh.sh "$BOOT_LOG" 300 "$BOOT_PID")"
echo "==> guest up on port $PORT"

# Fedora mounts btrfs discard=async and runs fstrim.timer; without this the guest returns the
# payload unprompted and the probe measures nothing.
g "sudo mount -o remount,nodiscard / && findmnt -no FSTYPE,OPTIONS /"
g "sudo systemctl stop fstrim.timer 2>/dev/null || true"

floor="$(allocated_mib)"
echo "floor:            $floor MiB"

g "sudo dd if=/dev/urandom of=/var/tmp/trim-payload bs=1M count=$PAYLOAD_MIB status=none; sync"
filled="$(allocated_mib)"
echo "after write:      $filled MiB  (+$((filled - floor)))"

g "sudo rm -f /var/tmp/trim-payload; sync"
sleep 5
deleted="$(allocated_mib)"
echo "after delete:     $deleted MiB  (+$((deleted - floor)); should still be held)"

echo
# Each cycle's `-m` is the minimum free-extent size fstrim will bother discarding. The
# supervisor passes 1 MiB (qga::trim::MIN_EXTENT) to guest-fstrim; a bare `fstrim /` passes
# none. Running both against the same deleted payload is the whole point of this probe.
MINIMUMS=(${MINIMUMS:-1M 1M 0 0})
printf '%-7s %-7s %-11s %-11s %-15s %s\n' cycle min after-MiB freed-MiB cumulative-MiB walked
prev="$deleted"
for min in "${MINIMUMS[@]}"; do
    i=$((${i:-0} + 1))
    walked="$(g "sudo fstrim -v -m $min / 2>&1 | tail -1" || echo "?")"
    sleep 15
    now="$(allocated_mib)"
    printf '%-7s %-7s %-11s %-11s %-15s %s\n' \
        "$i" "$min" "$now" "$((prev - now))" "$((deleted - now))" "$walked"
    prev="$now"
done

echo
echo "payload $PAYLOAD_MIB MiB; the test asserts cumulative > $((PAYLOAD_MIB / 2)) MiB after ONE trim"
echo "residue above floor: $((prev - floor)) MiB"

g "sudo sync; sudo systemctl poweroff" >/dev/null 2>&1 || true
sleep 10
