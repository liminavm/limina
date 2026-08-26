#!/usr/bin/env bash
# Install a mutter 50 built INSIDE the guest over the stock one, and prove it is the loaded copy.
#
#   install-mutter-50.sh <ssh-port> [build-dir]
#
# Two traps this encodes, both of which have cost a day before:
#
#  * libmutter-18.so.0.0.0 is loaded from /usr/lib64/ DIRECTLY, while every other libmutter*.so
#    lives in /usr/lib64/mutter-18/. Copying libmutter-18 into mutter-18/ leaves the file inert
#    while other pieces of the same build load fine, so the install looks half-alive.
#  * Install the FULL set from ONE build (cogl + clutter + mtk + mutter). Mixing our clutter with
#    Fedora's stock cogl is an untested ABI blend, and the instrumentation here lives in clutter.
#
# It ends by reading the loaded files out of the new gnome-shell's /proc/PID/maps and printing
# their mtime and size: a build that did not actually get loaded is the failure mode that makes a
# lever look inert, so verify the artifact at the path the process maps, never just the copy.
set -e -o pipefail
PORT="${1:?}"; BUILD="${2:-/mnt/build/src50/mutter-50.0/build}"
SSH=(ssh -p "$PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes
     -o ControlMaster=auto -o ControlPath=/tmp/cal-%r@%h:%p -o ControlPersist=900 claude@127.0.0.1)

"${SSH[@]}" "set -e
for f in \$(find $BUILD -name 'libmutter*.so.0.0.0' -not -path '*.p/*'); do
  b=\$(basename \"\$f\")
  if [ \"\$b\" = 'libmutter-18.so.0.0.0' ]; then
    dest=/usr/lib64/\$b
  else
    dest=/usr/lib64/mutter-18/\$b
  fi
  [ -e \"\$dest\" ] || { echo \"skip \$b (no stock counterpart at \$dest)\"; continue; }
  sudo install -m755 \"\$f\" /tmp/i-\$b && sudo mv /tmp/i-\$b \"\$dest\" && echo \"installed \$dest\"
done"

echo "== restarting gdm"
"${SSH[@]}" 'sudo systemctl restart gdm' || true
sleep 25

echo "== verifying the NEW shell actually mapped our files"
"${SSH[@]}" 'pid=$(pgrep -x gnome-shell | head -1); echo "gnome-shell pid=$pid"
  for f in $(grep -oE "/usr/lib64[^ ]*libmutter[^ ]*\.so[^ ]*" /proc/$pid/maps | sort -u); do
    stat -c "  %y  %10s  %n" "$f"
  done'
