#!/bin/bash
# Does a reconnect need a size CHANGE, or just the size FIELD present?
#
# This decides whether limina can fix the dynamic/fixed display modes on its own. Those modes
# may not dictate the guest's resolution, so they send a reconnect with no size and the identity
# goes stale on synoik. If re-asserting the size the guest ALREADY has is enough, the fix is one
# line on our side; if only a changed size works, it is synoik's to fix.
#
# Three arms, alternating identity every time so no step can pass by already being on target:
#   A. sizeless reconnect            (control -- expected stale on synoik)
#   B. reconnect asserting CURRENT size (the candidate fix)
#   C. sizeless again                (re-control, to show B was not a one-off ordering effect)
#
# usage: sizeless-vs-current-size.sh <ssh-port> <socket-path>
set -u
PORT=$1; SOCK=$2

push() { printf '%s\n' "$1" | nc -U -w 1 "$SOCK"; }

state() {
  ssh -o BatchMode=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
      -o ConnectTimeout=8 -p "$PORT" claude@127.0.0.1 \
      'gdbus call --session --dest org.gnome.Mutter.DisplayConfig --object-path /org/gnome/Mutter/DisplayConfig --method org.gnome.Mutter.DisplayConfig.GetCurrentState' 2>/dev/null
}
identity() { state | grep -oE "'[^']*', '0x[0-9A-Fa-f]+'\)" | head -1; }
current_mode() { state | grep -oE "'[0-9]+x[0-9]+@[0-9.]+'" | head -1 | tr -d "'"; }
current_size() { current_mode | cut -d@ -f1; }

# Two identities to alternate between; neither is the host's real display.
ident_a='refresh=60 dpi=140 vendor=LMN product=4001 serial=1111111111 name=PROBE%20Alpha'
ident_b='refresh=60 dpi=140 vendor=LMN product=4002 serial=2222222222 name=PROBE%20Beta'

arm() {
  local label=$1 with_size=$2
  local before size fields expect got
  before=$(identity)
  # Aim at whichever identity we are not currently on.
  case "$before" in
    *Alpha*) fields=$ident_b; expect="PROBE Beta" ;;
    *)       fields=$ident_a; expect="PROBE Alpha" ;;
  esac
  size=$(current_size)

  push 'display id=0 connected=0'
  sleep 0.1
  if [ "$with_size" = yes ]; then
    push "display id=0 connected=1 size=$size $fields"
  else
    push "display id=0 connected=1 $fields"
  fi
  sleep 12

  got=$(identity)
  case "$got" in
    *"$expect"*) echo "$label  size=$( [ "$with_size" = yes ] && echo "$size (UNCHANGED)" || echo none )  target=$expect  -> RE-READ" ;;
    *)           echo "$label  size=$( [ "$with_size" = yes ] && echo "$size (UNCHANGED)" || echo none )  target=$expect  -> STALE   [$got]" ;;
  esac
}

echo "starting identity: $(identity)   mode: $(current_mode)"
# Seed a known probe identity first so the alternation is well defined.
push 'display id=0 connected=0'; sleep 0.1
push "display id=0 connected=1 size=$(current_size) $ident_a"; sleep 12
echo "seeded: $(identity)"

arm "A sizeless      " no
arm "B current-size  " yes
arm "C sizeless again" no
