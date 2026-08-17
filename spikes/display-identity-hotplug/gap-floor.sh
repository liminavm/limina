#!/bin/bash
# Nail the shortest disconnect window that still makes the guest re-read the EDID.
# Every step is a genuine identity change, so no step can pass by already being there.
# usage: gap-floor.sh <ssh-port> <socket-path> <gap-seconds> [repeats]
set -u
PORT=$1; SOCK=$2; GAP=$3; REPS=${4:-2}

A='display id=0 size=2560x1440 refresh=60 dpi=125 vendor=LMN product=32795 serial=1816328933 name=BenQ%20LCD'
B='display id=0 size=3840x2160 refresh=60 dpi=163 vendor=LMN product=9999 serial=923604880 name=DELL%20P2723QE'

push() { printf '%s\n' "$1" | nc -U -w 1 "$SOCK"; }
identity() {
  ssh -o BatchMode=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
      -o ConnectTimeout=8 -p "$PORT" claude@127.0.0.1 \
      'gdbus call --session --dest org.gnome.Mutter.DisplayConfig --object-path /org/gnome/Mutter/DisplayConfig --method org.gnome.Mutter.DisplayConfig.GetCurrentState' 2>/dev/null \
    | grep -oE "'(BenQ LCD|DELL P2723QE)'" | head -1
}

start=$(identity)
echo "starting on: $start"
for i in $(seq 1 "$REPS"); do
  # Always aim at whichever identity we are NOT on, so the step is a real change.
  if [ "$(identity)" = "'BenQ LCD'" ]; then line=$B; expect="DELL P2723QE"; else line=$A; expect="BenQ LCD"; fi
  push 'display id=0 connected=0'
  sleep "$GAP"
  push "$line"
  push 'display id=0 connected=1'
  sleep 10
  got=$(identity)
  case "$got" in
    *"$expect"*) echo "gap=${GAP}s rep$i  target=$expect  -> RE-READ" ;;
    *)           echo "gap=${GAP}s rep$i  target=$expect  -> STALE [$got]" ;;
  esac
done
