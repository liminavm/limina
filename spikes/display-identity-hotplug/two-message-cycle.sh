#!/bin/bash
# Does a TWO-message cycle suffice? (a) connected=0, then (b) the new EDID and
# connected=1 folded into one command — versus the three-message form.
# Every step targets whichever identity we are NOT on, so no step can pass by inertia.
# usage: two-message-cycle.sh <ssh-port> <socket-path> [repeats]
set -u
PORT=$1; SOCK=$2; REPS=${3:-3}

A='size=2560x1440 refresh=60 dpi=125 vendor=LMN product=32795 serial=1816328933 name=BenQ%20LCD'
B='size=3840x2160 refresh=60 dpi=163 vendor=LMN product=9999 serial=923604880 name=DELL%20P2723QE'

push() { printf '%s\n' "$1" | nc -U -w 1 "$SOCK"; }
identity() {
  ssh -o BatchMode=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
      -o ConnectTimeout=8 -p "$PORT" claude@127.0.0.1 \
      'gdbus call --session --dest org.gnome.Mutter.DisplayConfig --object-path /org/gnome/Mutter/DisplayConfig --method org.gnome.Mutter.DisplayConfig.GetCurrentState' 2>/dev/null \
    | grep -oE "'(BenQ LCD|DELL P2723QE)'" | head -1
}

echo "starting on: $(identity)"
for i in $(seq 1 "$REPS"); do
  if [ "$(identity)" = "'BenQ LCD'" ]; then fields=$B; expect="DELL P2723QE"; else fields=$A; expect="BenQ LCD"; fi
  # Two messages: down, then the whole new display state including connected=1.
  push 'display id=0 connected=0'
  push "display id=0 connected=1 $fields"
  sleep 10
  got=$(identity)
  case "$got" in
    *"$expect"*) echo "two-message rep$i  target=$expect  -> RE-READ" ;;
    *)           echo "two-message rep$i  target=$expect  -> STALE [$got]" ;;
  esac
done
