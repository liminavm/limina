#!/usr/bin/env bash
# One A/B cycle: clear notifications, post two fresh ones, open the clock menu (which is where the
# corruption is near-deterministic on the vrend path), let the open animation finish, dump the
# scanout, close the menu again.
#   ab-cycle.sh <ssh-port> <cycles> <tag> <surface-ids...>
set -u
PORT="$1"; CYCLES="$2"; TAG="$3"; shift 3; IDS="$*"
HERE="$(cd "$(dirname "$0")" && pwd)"
SSH=(ssh -p "$PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes
     -o ControlMaster=auto -o ControlPath=/tmp/ab-%r@%h:%p -o ControlPersist=120 claude@127.0.0.1)
GUEST_ENV="export YDOTOOL_SOCKET=/tmp/.ydotool_socket;
    export XDG_RUNTIME_DIR=/run/user/\$(id -u);
    export DBUS_SESSION_BUS_ADDRESS=unix:path=\$XDG_RUNTIME_DIR/bus;"
mkdir -p "$TAG"
for c in $(seq 1 "$CYCLES"); do
    "${SSH[@]}" "$GUEST_ENV ydotool key 1:1 1:0" >/dev/null 2>&1          # Esc: close any menu
    "${SSH[@]}" "$GUEST_ENV for n in \$(seq 1 400); do gdbus call --session \
        --dest org.freedesktop.Notifications --object-path /org/freedesktop/Notifications \
        --method org.freedesktop.Notifications.CloseNotification \$n; done" >/dev/null 2>&1
    "${SSH[@]}" "$GUEST_ENV notify-send 'CYC$c-one' 'AAAA BBBB CCCC DDDD $c';
        notify-send 'CYC$c-two' 'EEEE FFFF GGGG HHHH $c'" >/dev/null 2>&1
    sleep 1
    "${SSH[@]}" "$GUEST_ENV ydotool key 125:1 47:1 47:0 125:0" >/dev/null 2>&1   # Super+V: clock menu
    sleep 2.5
    for sid in $IDS; do "$HERE/dumpone.sh" "$sid" "$TAG/c$c-$sid.png"; done
    echo "cycle $c captured"
done
