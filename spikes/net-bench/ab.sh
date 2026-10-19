#!/usr/bin/env bash
# The whole battery against one booted guest, for comparing NIC backends on the same image.
# Usage: ab.sh <tag> <worker-pid> <gvproxy-pid|-> <host-bind-addr> <guest-ssh-host> <guest-ssh-port>
#   gvproxy: ab.sh gvproxy <w> <g> 127.0.0.1 127.0.0.1 <fwd-port>   (guest sees the host as .254)
#   vmnet:   ab.sh vmnet <w> - <bridge addr> <guest addr> 22
# Env: OUT (default ./ab-<tag>), SECS (iperf, default 20), SSH_BYTES (default 4 GiB).
set -euo pipefail
TAG=$1; W=$2; G=$3; BIND=$4; export SSH_HOST=$5 SSH_PORT=$6
HERE=$(cd "$(dirname "$0")" && pwd)
export OUT=${OUT:-$PWD/ab-$TAG} SECS=${SECS:-20} SSH_BYTES=${SSH_BYTES:-4294967296}
mkdir -p "$OUT"
# The guest reaches the host's 127.0.0.1 through gvproxy as 192.168.127.254; on vmnet it dials
# the bridge address itself.
export IPERF_HOST=$([ "$BIND" = 127.0.0.1 ] && echo 192.168.127.254 || echo "$BIND")
SSH=(/usr/bin/ssh -p "$SSH_PORT" -o BatchMode=yes -o StrictHostKeyChecking=no
     -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR "claude@$SSH_HOST")
cpu_s() { ps -o time= -p "$1" | awk -F'[:.]' '{ print ($1*60 + $2) + $3/100 }'; }

iperf3 -s -B "$BIND" -p 5201 > "$OUT/iperf-server.log" 2>&1 & IS=$!
python3 "$HERE/rr.py" serve 5202 "$BIND" & RS=$!
trap 'kill $IS $RS 2>/dev/null' EXIT
"${SSH[@]}" "cat > /tmp/rr.py" < "$HERE/rr.py"
sleep 1

{
  echo "== $TAG on $(sw_vers -productVersion), $(date -u +%FT%TZ)"
  echo "-- idle: CPU seconds over 30 s, quiet guest"
  w0=$(cpu_s "$W"); g0=$([ "$G" = - ] && echo 0 || cpu_s "$G"); sleep 30
  echo "worker $(echo "$(cpu_s "$W") - $w0" | bc) s, gvproxy $([ "$G" = - ] && echo - || echo "$(cpu_s "$G") - $g0" | bc) s"
  echo "-- memory (RSS KiB): worker $(ps -o rss= -p "$W"), gvproxy $([ "$G" = - ] && echo - || ps -o rss= -p "$G")"
  for run in 1 2; do
    echo "-- latency, run $run"
    "${SSH[@]}" -n "python3 /tmp/rr.py client $IPERF_HOST 5202 10000"
  done
  for mode in "iperf" "iperf -R"; do
    for run in 1 2; do
      echo "-- $mode, run $run"
      bash "$HERE/netbench.sh" "$TAG-${mode// /}-$run" "$W" "$G" $mode
    done
  done
  for mode in ssh-g2h ssh-h2g; do
    for run in 1 2; do
      echo "-- $mode, run $run"
      bash "$HERE/netbench.sh" "$TAG-$mode-$run" "$W" "$G" "$mode"
    done
  done
} 2>&1 | tee "$OUT/summary.txt"
