#!/usr/bin/env bash
# One profiled iperf3 run over the LIMINA_VSOCK_BENCH path. See RESULTS.md for the setup.
# Usage: vsockbench.sh <tag> <worker-pid> [extra iperf3 client args, e.g. -R or -P 4]
# Env: OUT (output dir, default .), SSH_PORT (default 2222), SSH_USER (default claude).
#   guest iperf3 -c 127.0.0.1:5201 -> guest socat TCP->VSOCK-CONNECT:2:<bench port>
#   -> libkrun muxer -> host bench socket -> host socat -> host iperf3 -s :5201
set -euo pipefail
TAG=$1; W=$2; shift 2
OUT=${OUT:-.}
HERE=$(cd "$(dirname "$0")" && pwd)
SSH=(ssh -n -p "${SSH_PORT:-2222}" -o BatchMode=yes -o StrictHostKeyChecking=no
     -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR "${SSH_USER:-claude}@127.0.0.1")

"${SSH[@]}" "pkill -x iperf3; true"
sleep 1
"${SSH[@]}" "iperf3 -c 127.0.0.1 -p 5201 -t 30 -J $*" > "$OUT/iperf-$TAG.json" 2>&1 &
IP=$!
sleep 8
echo "worker %cpu under load:"; for _ in 1 2 3; do ps -o %cpu= -p "$W"; sleep 1; done
ps -M -p "$W" > "$OUT/threads-$TAG.txt"
"${SSH[@]}" "top -b -n 1 -o %CPU | head -n 20" > "$OUT/guest-top-$TAG.txt" 2>&1
sample "$W" 10 -file "$OUT/worker-$TAG.sample" > /dev/null 2>&1
wait "$IP"
python3 - "$OUT/iperf-$TAG.json" <<'EOF'
import json, sys
d = json.load(open(sys.argv[1]))
if "error" in d:
    sys.exit("iperf3 error: " + d["error"])
e = d["end"]
cpu = e["cpu_utilization_percent"]
print("%s: sent %.3f Gbit/s, received %.3f Gbit/s; iperf cpu guest %.1f%% / host %.1f%%" % (
    sys.argv[1].rsplit("/", 1)[-1], e["sum_sent"]["bits_per_second"] / 1e9,
    e["sum_received"]["bits_per_second"] / 1e9, cpu["host_total"], cpu["remote_total"]))
EOF
# threadacct's idle set misses several of the worker's wait primitives, so its host-busy column
# overcounts blocked threads; read the sample's call trees for the vsock threads instead.
python3 "$HERE/../venus-draw-probe/threadacct.py" "$OUT/worker-$TAG.sample" 10 \
    > "$OUT/threadacct-$TAG.txt" 2>&1 || true
