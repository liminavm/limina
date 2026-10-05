#!/bin/bash
# Record what else the host was doing while a pass ran: the top CPU consumers every 10 s.
#
#   hostmon.sh <outfile> <pid-to-outlive>    exits when that pid does
set -euo pipefail
out="${1:?outfile}"
watch="${2:?pid}"
: >"$out"
while kill -0 "$watch" 2>/dev/null; do
    {
        echo "## $(date '+%T') load $(sysctl -n vm.loadavg)"
        top -l 2 -s 1 -n 8 -o cpu -stats pid,command,cpu,th | awk '/^PID/{n++} n==2'
    } >>"$out"
    sleep 9
done
