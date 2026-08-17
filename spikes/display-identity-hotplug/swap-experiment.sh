#!/bin/bash
# In-place EDID identity swap over the display-control socket, then swap back.
# Reads the guest's monitors.xml + applied scale at every step.
#
# usage: swap-experiment.sh <ssh-port> <socket-path> <label> [cycle]
#   cycle = do a genuine disconnect -> new EDID -> reconnect instead of an in-place swap
set -u
PORT=$1; SOCK=$2; LABEL=$3; MODE=${4:-inplace}

# Identity A: the real host display (BenQ LCD), as the supervisor pushes it today.
A='display id=0 size=2560x1440 refresh=60 dpi=125 vendor=LMN product=32795 serial=1816328933 name=BenQ%20LCD'
# Identity B: a synthetic second host display (a 4K Dell), as a dock would present.
B='display id=0 size=3840x2160 refresh=60 dpi=163 vendor=LMN product=9999 serial=923604880 name=DELL%20P2723QE'

push() { printf '%s\n' "$1" | nc -U -w 1 "$SOCK"; }

guest() {
  ssh -o BatchMode=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
      -o ConnectTimeout=8 -p "$PORT" claude@127.0.0.1 "$1" 2>/dev/null
}

report() {
  echo "===== [$LABEL/$MODE] $1 ====="
  echo "-- applied logical monitors (scale, identity) --"
  guest 'gdbus call --session --dest org.gnome.Mutter.DisplayConfig --object-path /org/gnome/Mutter/DisplayConfig --method org.gnome.Mutter.DisplayConfig.GetCurrentState' \
    | tr ']' '\n' | grep -oE "\(0, 0, [0-9.]+.*" | head -3
  echo "-- monitors.xml stanzas --"
  guest 'grep -cE "<configuration>" ~/.config/monitors.xml 2>/dev/null || echo 0'
  echo "-- monitors.xml identities + scales --"
  guest 'grep -E "<scale>|<vendor>|<product>|<serial>" ~/.config/monitors.xml 2>/dev/null'
}

report "baseline"

if [ "$MODE" = cycle ]; then
  echo ">>> pushing a genuine unplug, then identity B, then replug"
  push 'display id=0 connected=0'
  sleep 3
  push "$B"
  sleep 1
  push 'display id=0 connected=1'
else
  echo ">>> pushing identity B in place"
  push "$B"
fi
sleep 12
report "after swap to B (DELL P2723QE)"

if [ "$MODE" = cycle ]; then
  echo ">>> cycling back to identity A"
  push 'display id=0 connected=0'
  sleep 3
  push "$A"
  sleep 1
  push 'display id=0 connected=1'
else
  echo ">>> pushing identity A back in place"
  push "$A"
fi
sleep 12
report "after swap back to A (BenQ LCD)"
