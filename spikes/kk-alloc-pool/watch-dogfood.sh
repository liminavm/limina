#!/usr/bin/env bash
# Mirror a remote Mac's KosmicKrisp pool state locally, read-only over ssh.
#
# Two things make this worth running rather than reading the files afterwards. The driver's
# snapshot is REWRITTEN in place, so a restart after a crash destroys the crashed run's final
# state — which is exactly what a post-mortem wants. And the trajectory matters as much as the
# endpoint: "compute crossed its budget two minutes before the fault" is something only a time
# series can say.
#
# Nothing is written on the remote; this is cat and grep over ssh.
#
#   watch-dogfood.sh <user@host> <vm-name> <out-dir> [interval-seconds]
set -uo pipefail
target=${1:?user@host}; vm=${2:?VM bundle name, e.g. Dev}; out=${3:?output dir}; iv=${4:-20}
mkdir -p "$out"
pool="$out/kk-pool.tsv"; det="$out/detectors.log"; meta="$out/worker.log"
logs="~/Library/Application\\ Support/limina/VMs/${vm}.liminavm/logs"
[ -s "$pool" ] || printf '# utc\tpid\tline\n' > "$pool"

while true; do
  now=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  # One ssh per tick carries everything: the snapshots, the worker pid, and any detector hits.
  snap=$(ssh -o BatchMode=yes -o ConnectTimeout=10 "$target" \
    "bash -lc 'cat $logs/kk-pool.txt* 2>/dev/null; echo \"---PID---\"; pgrep -f \"Limina.app/Contents/MacOS/limina-vmm\" | head -1; echo \"---DET---\"; grep -hE \"USE AFTER DESTROY|UNMATCHED DISCHARGE\" $logs/supervisor.log 2>/dev/null | tail -5'" 2>/dev/null)

  if [ -z "$snap" ]; then
    printf '%s\t-\tUNREACHABLE\n' "$now" >> "$pool"
    sleep "$iv"; continue
  fi
  pid=$(printf '%s' "$snap" | sed -n '/---PID---/,/---DET---/p' | sed '1d;$d' | tr -d '[:space:]')
  printf '%s' "$snap" | sed -n '1,/---PID---/p' | sed '$d' | while IFS= read -r l; do
    [ -n "$l" ] && printf '%s\t%s\t%s\n' "$now" "${pid:-none}" "$l" >> "$pool"
  done
  d=$(printf '%s' "$snap" | sed -n '/---DET---/,$p' | sed '1d')
  [ -n "$d" ] && printf '%s\n%s\n' "$now" "$d" >> "$det"
  printf '%s pid=%s\n' "$now" "${pid:-none}" >> "$meta"
  sleep "$iv"
done
