#!/bin/bash
# The user-facing behaviour, end to end: give each host display its own scale, then move the
# window back and forth and check that each display's remembered scale is re-applied and that
# monitors.xml accumulates one stanza per display.
#
# usage: per-display-memory.sh <ssh-port> <x-on-A> <y-on-A> <x-on-B> <y-on-B> <scale-on-B>
set -u
PORT=$1; AX=$2; AY=$3; BX=$4; BY=$5; BSCALE=$6

guest() {
  ssh -o BatchMode=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
      -o ConnectTimeout=8 -p "$PORT" claude@127.0.0.1 "$1" 2>/dev/null
}

# (identity, applied scale) as one line.
state() {
  guest 'gdbus call --session --dest org.gnome.Mutter.DisplayConfig --object-path /org/gnome/Mutter/DisplayConfig --method org.gnome.Mutter.DisplayConfig.GetCurrentState' \
    | grep -oE "'[^']*', '0x[0-9A-Fa-f]+'\)|\(0, 0, [0-9.]+" | tr '\n' ' '
}

stanzas() { guest 'grep -cE "<configuration>" ~/.config/monitors.xml 2>/dev/null || echo 0'; }
saved()   { guest 'grep -E "<scale>|<product>" ~/.config/monitors.xml 2>/dev/null | tr -d " "' | tr '\n' ' '; }

move() {
  osascript >/dev/null 2>&1 <<EOF
tell application "System Events"
  repeat with p in (every process whose name contains "limina")
    try
      set position of window 1 of p to {$1, $2}
    end try
  end repeat
end tell
EOF
  sleep 12
}

# Persist a scale for whatever display is live now — this is what the Settings panel does.
apply_scale() {
  local want=$1
  local serial mode
  serial=$(guest 'gdbus call --session --dest org.gnome.Mutter.DisplayConfig --object-path /org/gnome/Mutter/DisplayConfig --method org.gnome.Mutter.DisplayConfig.GetCurrentState' | grep -oE '^\(uint32 [0-9]+' | grep -oE '[0-9]+$')
  mode=$(guest 'gdbus call --session --dest org.gnome.Mutter.DisplayConfig --object-path /org/gnome/Mutter/DisplayConfig --method org.gnome.Mutter.DisplayConfig.GetCurrentState' | grep -oE "'[0-9]+x[0-9]+@[0-9.]+'" | head -1 | tr -d "'")
  echo "  applying scale $want on mode $mode (serial $serial, method 2 = persistent)"
  guest "gdbus call --session --dest org.gnome.Mutter.DisplayConfig \
    --object-path /org/gnome/Mutter/DisplayConfig \
    --method org.gnome.Mutter.DisplayConfig.ApplyMonitorsConfig \
    $serial 2 \"[(0, 0, $want, uint32 0, true, [('Virtual-1', '$mode', @a{sv} {})])]\" \"@a{sv} {}\"" \
    || echo "  (apply failed)"
  sleep 6
}

echo "===== 1. on display A ====="
move "$AX" "$AY"; echo "  $(state)"; echo "  stanzas=$(stanzas)  saved: $(saved)"

echo "===== 2. move to display B, give it its own scale ====="
move "$BX" "$BY"; echo "  $(state)"
apply_scale "$BSCALE"
echo "  $(state)"; echo "  stanzas=$(stanzas)  saved: $(saved)"

echo "===== 3. back to A — its own scale should return ====="
move "$AX" "$AY"; echo "  $(state)"; echo "  stanzas=$(stanzas)  saved: $(saved)"

echo "===== 4. to B again — B's scale should return ====="
move "$BX" "$BY"; echo "  $(state)"; echo "  stanzas=$(stanzas)  saved: $(saved)"
