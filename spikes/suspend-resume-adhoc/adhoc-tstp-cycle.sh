#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# ad-hoc suspend/resume cycle using the PROPER trigger: SIGTSTP to the supervisor
# (suspend bracket: pulse sleep button, wait for s2idle quiesce, snapshot, exit 126).
set -u
cd ~/Projects/limina
EXTRA="$*"
reap() { for p in $(ps -eo pid,command | grep -E "[c]argo xtask run|[t]arget/debug/limina( |-vmm)" | awk '{print $1}'); do kill $p 2>/dev/null; done; sleep 2; }
preflight() {
  n=$(ps -eo command | grep -cE "[t]arget/debug/limina( |-vmm)"); [ "$n" -eq 0 ] || { echo "PREFLIGHT-FAIL: $n limina procs"; exit 2; }
  nc -z 127.0.0.1 2299 2>/dev/null && { echo "PREFLIGHT-FAIL: port 2299 busy"; exit 2; }
  return 0
}
wait_ssh() { SECONDS=0; until ssh -p 2299 -o BatchMode=yes -o ConnectTimeout=3 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null claude@127.0.0.1 true 2>/dev/null; do sleep 3; [ $SECONDS -gt ${1:-300} ] && return 1; done; return 0; }
guest_ssh() { ssh -p 2299 -o BatchMode=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null claude@127.0.0.1 "$@" 2>/dev/null; }
preflight
rm -f imago-validate.snap imago-validate.bin.consumed
rm -f imago-validate.raw; cp -c Fedora-Workstation-44.enhanced.raw imago-validate.raw
cargo xtask run --disk imago-validate.raw -- --ssh-port 2299 --snapshot-file ~/Projects/limina/imago-validate.snap $EXTRA > /tmp/adhoc-leg1.log 2>&1 &
wait_ssh 300 || { echo BOOT-TIMEOUT; reap; exit 1; }
BOOTID1=$(guest_ssh "cat /proc/sys/kernel/random/boot_id"); echo "booted boot_id=$BOOTID1"
WPID=$(ps -eo pid,command | grep "[t]arget/debug/limina-vmm --cpus" | awk '{print $1}')
SUPPID=$(ps -eo pid,command | grep "[t]arget/debug/limina --vmm-bin" | awk '{print $1}')
kill -TSTP "$SUPPID"
SECONDS=0
until ! ps -p "$WPID" >/dev/null 2>&1; do
  sleep 2
  [ $SECONDS -gt 90 ] && { echo BRACKET-REFUSED-GUEST-STILL-RUNNING; reap; exit 1; }
done
while ps -p "$SUPPID" >/dev/null 2>&1; do sleep 1; done
[ -s imago-validate.snap ] || { echo NO-SNAPSHOT-WRITTEN; exit 1; }
echo suspended-via-bracket
preflight || exit 2
cargo xtask run --disk imago-validate.raw -- --ssh-port 2299 --snapshot-file ~/Projects/limina/imago-validate.snap $EXTRA > /tmp/adhoc-leg2.log 2>&1 &
wait_ssh 180 || { echo RESUME-UNREACHABLE; reap; exit 1; }
BOOTID2=$(guest_ssh "cat /proc/sys/kernel/random/boot_id")
echo "resumed boot_id=$BOOTID2"
if [ "$BOOTID1" = "$BOOTID2" ]; then echo TRUE-RESUME; else echo COLD-BOOTED; fi
