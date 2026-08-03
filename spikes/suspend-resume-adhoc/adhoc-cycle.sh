#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

set -u
cd ~/Projects/limina
EXTRA="$*"
reap() { for p in $(ps -eo pid,command | grep -E "[t]arget/debug/limina( |-vmm)" | awk '{print $1}'); do kill $p 2>/dev/null; done; sleep 2; }
preflight() {
  n=$(ps -eo command | grep -cE "[t]arget/debug/limina( |-vmm)"); [ "$n" -eq 0 ] || { echo "PREFLIGHT-FAIL: $n limina procs"; exit 2; }
  nc -z 127.0.0.1 2299 2>/dev/null && { echo "PREFLIGHT-FAIL: port 2299 busy"; exit 2; }
  return 0
}
one_worker() { c=$(ps -eo command | grep -c "[t]arget/debug/limina-vmm --cpus"); [ "$c" -eq 1 ] || { echo "ASSERT-FAIL: $c workers"; reap; exit 2; }; }
wait_ssh() { SECONDS=0; until ssh -p 2299 -o BatchMode=yes -o ConnectTimeout=3 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null claude@127.0.0.1 true 2>/dev/null; do sleep 3; [ $SECONDS -gt ${1:-300} ] && return 1; done; return 0; }
preflight
rm -f imago-validate.snap; rm -f imago-validate.raw; cp -c Fedora-Workstation-44.enhanced.raw imago-validate.raw
cargo xtask run --disk imago-validate.raw -- --ssh-port 2299 --snapshot-file ~/Projects/limina/imago-validate.snap $EXTRA > /tmp/adhoc-leg1.log 2>&1 &
wait_ssh 300 || { echo BOOT-TIMEOUT; reap; exit 1; }
echo booted; one_worker
WPID=$(ps -eo pid,command | grep "[t]arget/debug/limina-vmm --cpus" | awk '{print $1}')
kill -USR1 "$WPID"
until ! ps -p "$WPID" >/dev/null 2>&1; do sleep 1; done
SUPPID=$(ps -eo pid,command | grep "[t]arget/debug/limina --vmm-bin" | awk '{print $1}')
[ -n "$SUPPID" ] && while ps -p "$SUPPID" >/dev/null 2>&1; do sleep 1; done
[ -s imago-validate.snap ] || { echo NO-SNAPSHOT-WRITTEN; exit 1; }
echo suspended
preflight || exit 2
cargo xtask run --disk imago-validate.raw -- --ssh-port 2299 --snapshot-file ~/Projects/limina/imago-validate.snap $EXTRA > /tmp/adhoc-leg2.log 2>&1 &
if wait_ssh 180; then echo RESUMED-OK; else echo RESUME-UNREACHABLE; reap; exit 1; fi
