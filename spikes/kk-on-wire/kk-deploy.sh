#!/bin/bash
# Copies the guest KK build and kk-probe into a guest and runs a command there with the driver
# selected. Usage: kk-deploy.sh <ssh-port> [command...]
# The test images' credentials are in docs/images.md (GUEST_USER / GUEST_PASS override them).
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
port=$1
shift
user=${GUEST_USER:-claude}
pass=${GUEST_PASS:-claudiusrobotus}
O=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR)
printf '{"ICD":{"api_version":"1.4.363","library_path":"/tmp/kk/libvulkan_kosmickrisp.so"},"file_format_version":"1.0.1"}\n' \
  > "$here/out/kk_limina_icd.json"
sshpass -p "$pass" ssh -p "$port" "${O[@]}" "$user@127.0.0.1" 'mkdir -p /tmp/kk' || exit 1
files=("$here/out/libvulkan_kosmickrisp.so" "$here/out/kk_limina_icd.json")
[ -f "$here/out/kk-probe" ] && files+=("$here/out/kk-probe")
sshpass -p "$pass" scp -P "$port" "${O[@]}" "${files[@]}" "$user@127.0.0.1:/tmp/kk/" || exit 1
[ $# -gt 0 ] || exit 0
gtimeout --kill-after=10 300 sshpass -p "$pass" ssh -p "$port" "${O[@]}" "$user@127.0.0.1" \
  "cd /tmp/kk && export VK_DRIVER_FILES=/tmp/kk/kk_limina_icd.json; $*"
