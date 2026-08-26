#!/usr/bin/env bash
set -u
OUT="$1"; N="${2:-6}"
SSH=(ssh -p 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes
     -o ControlMaster=auto -o ControlPath=/tmp/lp-%r@%h:%p -o ControlPersist=200 claude@127.0.0.1)
GE='export YDOTOOL_SOCKET=/tmp/.ydotool_socket; export XDG_RUNTIME_DIR=/run/user/$(id -u); export DBUS_SESSION_BUS_ADDRESS=unix:path=$XDG_RUNTIME_DIR/bus;'
"${SSH[@]}" "$GE rm -f ~/Pictures/Screenshots/*.png; ydotool key 1:1 1:0" >/dev/null 2>&1
sleep 1.5
for i in $(seq 1 "$N"); do
  "${SSH[@]}" "$GE for n in \$(seq 1 6000); do gdbus call --session --dest org.freedesktop.Notifications --object-path /org/freedesktop/Notifications --method org.freedesktop.Notifications.CloseNotification \$n; done" >/dev/null 2>&1
  "${SSH[@]}" "$GE ydotool key 62:1 62:0" >/dev/null 2>&1
  sleep 0.6
  "${SSH[@]}" "$GE notify-send -a Software -i software-update-available-symbolic 'Critical Updates L$i' 'Install critical updates as soon as possible'" >/dev/null 2>&1
  sleep 1.2
  "${SSH[@]}" "$GE ydotool key 42:1 99:1 99:0 42:0" >/dev/null 2>&1
  sleep 2.0
  f=$("${SSH[@]}" 'ls -t ~/Pictures/Screenshots/*.png 2>/dev/null | head -1' | tr -d '\r')
  if [ -n "$f" ]; then
    scp -q -P 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ControlPath=/tmp/lp-%r@%h:%p claude@127.0.0.1:"$f" "$OUT/lp-$i.png" && echo "got lp-$i.png"
    "${SSH[@]}" "rm -f '$f'" >/dev/null 2>&1
  else echo "no screenshot $i"; fi
  sleep 2
done
