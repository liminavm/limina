#!/bin/bash
# How short can the disconnect window be and still make the guest re-read the EDID?
# Alternates between two identities, one gap length per step, and reports which
# identity the compositor believes it is on afterwards.
#
# usage: gap-sweep.sh <ssh-port> <socket-path>
set -u
PORT=$1; SOCK=$2

A='display id=0 size=2560x1440 refresh=60 dpi=125 vendor=LMN product=32795 serial=1816328933 name=BenQ%20LCD'
B='display id=0 size=3840x2160 refresh=60 dpi=163 vendor=LMN product=9999 serial=923604880 name=DELL%20P2723QE'

push() { printf '%s\n' "$1" | nc -U -w 1 "$SOCK"; }

identity() {
  ssh -o BatchMode=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
      -o ConnectTimeout=8 -p "$PORT" claude@127.0.0.1 \
      'gdbus call --session --dest org.gnome.Mutter.DisplayConfig --object-path /org/gnome/Mutter/DisplayConfig --method org.gnome.Mutter.DisplayConfig.GetCurrentState' 2>/dev/null \
    | grep -oE "'(BenQ LCD|DELL P2723QE)', '0x[0-9A-Fa-f]+'" | head -1
}

# Alternate target each step so every step is a real change.
step=0
for gap in 0 0.05 0.1 0.25 0.5 1 2; do
  step=$((step + 1))
  if [ $((step % 2)) -eq 1 ]; then want=B; line=$B; expect="DELL P2723QE"; else want=A; line=$A; expect="BenQ LCD"; fi

  if [ "$gap" = 0 ]; then
    # All three commands in a single write: the guest may never observe the down state.
    printf 'display id=0 connected=0\n%s\ndisplay id=0 connected=1\n' "$line" | nc -U -w 1 "$SOCK"
  else
    push 'display id=0 connected=0'
    sleep "$gap"
    push "$line"
    push 'display id=0 connected=1'
  fi

  sleep 10
  got=$(identity)
  case "$got" in
    *"$expect"*) verdict="RE-READ" ;;
    *)           verdict="STALE" ;;
  esac
  echo "gap=${gap}s  target=$want ($expect)  ->  $verdict   [$got]"
done
