#!/usr/bin/env bash
# Select an arm for the instrumented mutter and restart the session into it.
#
#   glyph-arm.sh <ssh-port> [VAR=VAL ...]      # no vars = stock behaviour, instrumentation off
#
# The session gnome-shell runs as org.gnome.Shell@user.service under the user manager, so a unit
# drop-in is what reaches it. /etc/environment.d/ does NOT: the user manager reads that only when
# it starts, and restarting gdm leaves the user manager alive -- the file lands, the shell never
# sees it, and the arm silently runs stock while reporting success (measured: log=0 after a gdm
# restart with the file in place).
set -e -o pipefail
PORT="${1:?}"; shift
SSH=(ssh -p "$PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes
     -o ControlMaster=auto -o ControlPath=/tmp/cal-%r@%h:%p -o ControlPersist=900 claude@127.0.0.1)
LINES=""
for kv in "$@"; do LINES="${LINES}Environment=${kv}\n"; done

"${SSH[@]}" "sudo rm -f /etc/environment.d/95-limina-glyph.conf
sudo mkdir -p /etc/systemd/user/org.gnome.Shell@user.service.d
printf '[Service]\n${LINES}' | sudo tee /etc/systemd/user/org.gnome.Shell@user.service.d/limina-glyph.conf >/dev/null
sudo systemctl restart gdm"
sleep 28
# The arm must evidence itself: the instrumentation prints its own log/sync state on first use.
"${SSH[@]}" 'sudo journalctl -b --since "-1min" 2>/dev/null | grep -a "instrumented mutter live" | tail -1'
