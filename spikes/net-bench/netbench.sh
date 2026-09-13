#!/usr/bin/env bash
# One profiled transfer over the gvproxy NAT path. See RESULTS.md for the setup.
# Usage: netbench.sh <tag> <worker-pid> <gvproxy-pid> iperf [extra iperf3 client args, e.g. -R]
#        netbench.sh <tag> <worker-pid> <gvproxy-pid> ssh-g2h|ssh-h2g
# Env: OUT (output dir, default .), SSH_PORT (default 2222), SSH_USER (default claude),
#      SECS (iperf duration, default 30), SSH_BYTES (ssh transfer size, default 8 GiB).
#   iperf: guest iperf3 -c 192.168.127.254 -> virtio-net -> libkrun -> unixgram -> gvproxy
#          netstack -> host 127.0.0.1:5201 (host iperf3 -s)
#   ssh:   host ssh -> 127.0.0.1:$SSH_PORT -> gvproxy forward -> guest sshd; the payload is
#          /dev/zero through cat on both ends, so the cipher and the path are what is measured.
set -euo pipefail
TAG=$1; W=$2; G=$3; MODE=$4; shift 4
OUT=${OUT:-.}
SECS=${SECS:-30}
SSH_BYTES=${SSH_BYTES:-8589934592}
HERE=$(cd "$(dirname "$0")" && pwd)
OPTS=(-p "${SSH_PORT:-2222}" -o BatchMode=yes -o StrictHostKeyChecking=no
      -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR)
SSH=(ssh "${OPTS[@]}" "${SSH_USER:-claude}@127.0.0.1")

counters() {
  # Net interrupts, then eth0 rx packets/bytes and tx packets/bytes. The guest names the device
  # `virtioN` in /proc/interrupts; net is virtio device id 0x0001.
  "${SSH[@]}" -n 'for d in /sys/bus/virtio/devices/virtio*; do
      [ "$(cat $d/device)" = 0x0001 ] && n=$(basename $d); done
    awk -v n="$n" "\$NF==n { s=0; for (i=2; i<=NF; i++) if (\$i ~ /^[0-9]+\$/) s+=\$i; printf \"%d \", s }" /proc/interrupts
    s=/sys/class/net/eth0/statistics
    echo $(cat $s/rx_packets $s/rx_bytes $s/tx_packets $s/tx_bytes)'
}

report_counters() {
  python3 - "$TAG" "$1" "$2" <<'EOF'
import sys
tag, a, b = sys.argv[1], sys.argv[2].split(), sys.argv[3].split()
d = [int(y) - int(x) for x, y in zip(a, b)]
per = lambda n, p: n // p if p else 0
print("%s: %d net interrupts; rx %d pkts, %d B/pkt; tx %d pkts, %d B/pkt" % (
    tag, d[0], d[1], per(d[2], d[1]), d[3], per(d[4], d[3])))
EOF
}

profile() {
  sleep 6
  echo "under load (worker %cpu, gvproxy %cpu):"
  for _ in 1 2 3; do echo "  $(ps -o %cpu= -p "$W") $(ps -o %cpu= -p "$G")"; sleep 1; done
  ps -M -p "$W" > "$OUT/threads-$TAG.txt"
  "${SSH[@]}" -n "top -b -n 1 -o %CPU | head -n 20" > "$OUT/guest-top-$TAG.txt" 2>&1
  sample "$W" 8 -file "$OUT/worker-$TAG.sample" > /dev/null 2>&1 &
  local s=$!
  sample "$G" 8 -file "$OUT/gvproxy-$TAG.sample" > /dev/null 2>&1
  wait "$s"
}

C0=$(counters)
case "$MODE" in
  iperf)
    "${SSH[@]}" -n "pkill -x iperf3; true"
    "${SSH[@]}" -n "iperf3 -c 192.168.127.254 -p 5201 -t $SECS -J $*" > "$OUT/iperf-$TAG.json" 2>&1 &
    P=$!
    profile
    wait "$P"
    python3 - "$OUT/iperf-$TAG.json" <<'EOF'
import json, sys
d = json.load(open(sys.argv[1]))
if "error" in d:
    sys.exit("iperf3 error: " + d["error"])
e = d["end"]
cpu = e["cpu_utilization_percent"]
retr = e["sum_sent"].get("retransmits", "-")
print("%s: sent %.3f Gbit/s, received %.3f Gbit/s, retransmits %s; iperf cpu guest %.1f%% / host %.1f%%" % (
    sys.argv[1].rsplit("/", 1)[-1], e["sum_sent"]["bits_per_second"] / 1e9,
    e["sum_received"]["bits_per_second"] / 1e9, retr, cpu["host_total"], cpu["remote_total"]))
print("bytes", e["sum_received"]["bytes"])
EOF
    ;;
  ssh-g2h|ssh-h2g)
    # Timed inside the backgrounded job: the profiling window is longer than a fast transfer,
    # so a stopwatch around `wait` measures the profile, not the path.
    if [ "$MODE" = ssh-g2h ]; then
      xfer() { "${SSH[@]}" -n "head -c $SSH_BYTES /dev/zero" > /dev/null; }
    else
      xfer() { head -c "$SSH_BYTES" /dev/zero | "${SSH[@]}" "cat > /dev/null"; }
    fi
    ( T0=$(python3 -c 'import time; print(time.time())'); xfer
      python3 -c "import time; s=time.time()-$T0; print('$TAG: $SSH_BYTES bytes ${MODE#ssh-} in %.1f s, %.3f Gbit/s' % (s, $SSH_BYTES*8/s/1e9))"
    ) &
    P=$!
    profile
    wait "$P"
    ;;
  *) echo "unknown mode $MODE" >&2; exit 2 ;;
esac
report_counters "$C0" "$(counters)"
python3 "$HERE/../venus-draw-probe/threadacct.py" "$OUT/worker-$TAG.sample" 8 \
    > "$OUT/threadacct-$TAG.txt" 2>&1 || true
