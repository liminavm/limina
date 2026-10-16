#!/bin/bash
# Copies out/ into a guest and runs the probe there. Usage: run.sh <ssh-port>
# The test images' credentials are in docs/images.md (GUEST_USER / GUEST_PASS override them).
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
port=$1
user=${GUEST_USER:-claude}
pass=${GUEST_PASS:-claudiusrobotus}
O=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR)
sshpass -p "$pass" ssh -p "$port" "${O[@]}" "$user@127.0.0.1" 'mkdir -p /tmp/mcp' || exit 1
sshpass -p "$pass" scp -P "$port" "${O[@]}" "$here/out/metal-compute-probe" \
  "$here/out/axpy.metallib" "$here/out/axpy.metal" "$user@127.0.0.1:/tmp/mcp/" || exit 1
gtimeout --kill-after=10 120 sshpass -p "$pass" ssh -p "$port" "${O[@]}" "$user@127.0.0.1" \
  'cd /tmp/mcp && ./metal-compute-probe axpy.metallib axpy.metal'
