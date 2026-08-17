#!/bin/bash
# Drive a REAL display migration: move the limina window onto another host display and read back
# which monitor the guest now believes it is on, plus what it remembered for that monitor.
#
# This is the end-to-end path — window position -> hostdisplay::describe -> migration_commands ->
# socket -> libkrun -> guest -- as opposed to pushing identities at the socket by hand.
#
# usage: migrate-window.sh <ssh-port> <x> <y> <label>
#   x,y = target position in AppKit's global coordinate space (see host-screens.swift)
set -u
PORT=$1; X=$2; Y=$3; LABEL=$4

guest() {
  ssh -o BatchMode=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
      -o ConnectTimeout=8 -p "$PORT" claude@127.0.0.1 "$1" 2>/dev/null
}

state() {
  guest 'gdbus call --session --dest org.gnome.Mutter.DisplayConfig --object-path /org/gnome/Mutter/DisplayConfig --method org.gnome.Mutter.DisplayConfig.GetCurrentState' \
    | grep -oE "\('Virtual-1', '[^,]*', '[^,]*', '[^)]*'\)|\(0, 0, [0-9.]+" | head -2 | tr '\n' ' '
}

echo "===== [$LABEL] before move ====="
echo "  guest: $(state)"

# AppKit's own coordinates: the window is the app's, and moving it across a screen boundary is
# what the supervisor's per-tick screen check notices. System Events uses top-left origin with y
# growing DOWNWARD, which is why the caller passes a converted y.
osascript <<EOF 2>&1 | head -3
tell application "System Events"
  set procs to every process whose name contains "limina"
  repeat with p in procs
    try
      set position of window 1 of p to {$X, $Y}
    end try
  end repeat
end tell
EOF

sleep 12
echo "===== [$LABEL] after move to ($X,$Y) ====="
echo "  guest: $(state)"
echo "  monitors.xml stanzas: $(guest 'grep -cE "<configuration>" ~/.config/monitors.xml 2>/dev/null || echo 0')"
guest 'grep -E "<scale>|<product>|<serial>" ~/.config/monitors.xml 2>/dev/null' | sed 's/^/    /'
