#!/bin/bash
# The guest half: copies out/ into a guest, installs clang if it is missing, builds the test
# against Apple's metal-cpp and the stand-in headers, links limina-metal, and runs it three times:
# the compute run, an autorelease pool used at thread exit, then a call limina-metal does not
# implement, which must abort naming it.
# Usage: run.sh <ssh-port>. The test images' credentials are in docs/images.md.
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
port=$1
user=${GUEST_USER:-claude}
pass=${GUEST_PASS:-claudiusrobotus}
O=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR)
g() { gtimeout --kill-after=10 "$1" sshpass -p "$pass" ssh -p "$port" "${O[@]}" "$user@127.0.0.1" "$2"; }
g 30 'mkdir -p /tmp/mcs' || exit 1
tar --no-mac-metadata --no-xattrs -C "$here/out" -cf - metal-cpp include kernels.metallib kernels.metal axpy.cpp liblimina_metal.a |
  gtimeout --kill-after=10 120 sshpass -p "$pass" ssh -p "$port" "${O[@]}" "$user@127.0.0.1" \
    'tar -C /tmp/mcs -xf -' || exit 1
g 900 "command -v clang++ >/dev/null || echo '$pass' | sudo -S dnf -y -q install clang" || exit 1
g 300 'cd /tmp/mcs && clang++ -std=c++17 -fblocks -O1 -g -Wno-ignored-attributes -ferror-limit=0 \
  -I include -I metal-cpp axpy.cpp liblimina_metal.a -lpthread -ldl -lm -lgcc_s -o axpy' || exit 1
echo "== run"
g 120 'cd /tmp/mcs && ./axpy kernels.metallib kernels.metal'
rc=$?
echo "== exit $rc"
echo "== a pool at thread exit"
g 60 'cd /tmp/mcs && ./axpy kernels.metallib kernels.metal thread-exit'
t=$?
echo "== exit $t"
[ $rc -eq 0 ] && rc=$t
echo "== a method limina-metal lacks"
g 60 'cd /tmp/mcs && { ./axpy kernels.metallib kernels.metal unknown; echo "== exit $? (SIGABRT is 134)"; }'
exit $rc
