#!/usr/bin/env bash
# The reproducing sequence: open the clock menu FIRST, then post notifications INTO the already-open
# list. Each new card animates ("grows") into place, and it is that animation that empties it — the
# user reports the card appears with text and is then emptied by the grow. Posting first and opening
# afterwards does NOT reproduce: the cards are already laid out when the popup appears.
#   ab-open-then-notify.sh <ssh-port> <count> <tag> <surface-ids...>
set -u
PORT="$1"; N="$2"; TAG="$3"; shift 3; IDS="$*"
HERE="$(cd "$(dirname "$0")" && pwd)"
SSH=(ssh -p "$PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes
     -o ControlMaster=auto -o ControlPath=/tmp/ab2-%r@%h:%p -o ControlPersist=180 claude@127.0.0.1)
GE="export YDOTOOL_SOCKET=/tmp/.ydotool_socket;
    export XDG_RUNTIME_DIR=/run/user/\$(id -u);
    export DBUS_SESSION_BUS_ADDRESS=unix:path=\$XDG_RUNTIME_DIR/bus;"
mkdir -p "$TAG"
"${SSH[@]}" "$GE ydotool key 1:1 1:0" >/dev/null 2>&1                       # close anything open
"${SSH[@]}" "$GE for n in \$(seq 1 1500); do gdbus call --session --dest org.freedesktop.Notifications \
    --object-path /org/freedesktop/Notifications --method \
    org.freedesktop.Notifications.CloseNotification \$n; done" >/dev/null 2>&1
sleep 1
"${SSH[@]}" "$GE ydotool key 125:1 47:1 47:0 125:0" >/dev/null 2>&1          # open the clock menu
sleep 2
for i in $(seq 1 "$N"); do
    "${SSH[@]}" "$GE notify-send 'INTO$i' 'MMMM WWWW MMMM WWWW $i'" >/dev/null 2>&1
    sleep 1.8
    for sid in $IDS; do "$HERE/dumpone.sh" "$sid" "$TAG/n$i-$sid.png"; done
    echo "posted $i into open list, captured"
done
