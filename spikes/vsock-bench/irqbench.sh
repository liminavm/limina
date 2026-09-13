#!/usr/bin/env bash
# Count the guest's vsock interrupts over a 10 s iperf3 run in each direction.
# The guest names the device `virtioN` in /proc/interrupts, never "vsock": find it by virtio
# device id 0x0013 in sysfs first.
# Env: SSH_PORT (default 2222), SSH_USER (default claude).
set -euo pipefail
OPTS=(-o BatchMode=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR)
SSH=(ssh -n -p "${SSH_PORT:-2222}" "${OPTS[@]}" "${SSH_USER:-claude}@127.0.0.1")
COUNT=$(mktemp -t irqcount)
trap 'rm -f "$COUNT"' EXIT
cat > "$COUNT" <<'G'
#!/usr/bin/env bash
for d in /sys/bus/virtio/devices/virtio*; do
  if [ "$(cat $d/device)" = "0x0013" ]; then n=$(basename $d); fi
done
awk -v n="$n" '$NF==n { s=0; for (i=2; i<=NF; i++) if ($i ~ /^[0-9]+$/) s+=$i; print n, s }' /proc/interrupts
G
scp -q -P "${SSH_PORT:-2222}" "${OPTS[@]}" "$COUNT" "${SSH_USER:-claude}@127.0.0.1:/tmp/irqcount.sh"
for dir in g2h h2g; do
  flag=""; [ "$dir" = h2g ] && flag="-R"
  "${SSH[@]}" "pkill -x iperf3; true"; sleep 1
  a=$("${SSH[@]}" "bash /tmp/irqcount.sh" | awk '{print $2}')
  bytes=$("${SSH[@]}" "iperf3 -c 127.0.0.1 -p 5201 -t 10 $flag -J" |
          python3 -c 'import json,sys; print(json.load(sys.stdin)["end"]["sum_received"]["bytes"])')
  b=$("${SSH[@]}" "bash /tmp/irqcount.sh" | awk '{print $2}')
  irqs=$((b - a))
  echo "$dir: $bytes bytes in 10 s, $irqs vsock interrupts, $((irqs / 10))/s," \
       "$((bytes / (irqs > 0 ? irqs : 1))) bytes per interrupt"
done
"${SSH[@]}" "rm -f /tmp/irqcount.sh"
