#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# Drive each idle-inhibit path in the seated session and print what limina-agent-session told
# the host after each step. Run as the seated user over ssh, with no guest input during the run.
# Usage: idle-paths.sh <desktop>   desktop = sway | kde | gnome | synoik (sway uses swaymsg to
# launch, the rest systemd-run; kde plays mpv through wlshm, which survives QPainter compositing)
set -u
export XDG_RUNTIME_DIR=/run/user/$(id -u)
export DBUS_SESSION_BUS_ADDRESS=unix:path=$XDG_RUNTIME_DIR/bus
LAUNCH=$1
if [ "$LAUNCH" = sway ]; then
  export SWAYSOCK=$(ls $XDG_RUNTIME_DIR/sway-ipc.*.sock | head -1)
fi
WAYLAND_DISPLAY=$(cd $XDG_RUNTIME_DIR && ls wayland-? | head -1)
export WAYLAND_DISPLAY

run() {  # start a GUI program inside the session
  if [ "$LAUNCH" = sway ]; then swaymsg -q exec "$*"
  else systemd-run --user -q --collect -E WAYLAND_DISPLAY=$WAYLAND_DISPLAY -E QT_QPA_PLATFORM=wayland -E MOZ_ENABLE_WAYLAND=1 $*
  fi
}
mark() { date +%s.%N > /tmp/idle-mark; echo "== $*"; }
reports() {  # what the helper said since the last mark
  journalctl --user -u limina-agent-session --no-pager -o short-unix --since "@$(cat /tmp/idle-mark)" \
    | grep -E "telling the host|idle inhibitors" || echo "   (no report)"
}

[ -f /tmp/clip.webm ] || ffmpeg -hide_banner -loglevel error -f lavfi \
  -i testsrc2=size=640x360:rate=30:duration=20 -c:v libvpx-vp9 -deadline realtime -an /tmp/clip.webm
# Firefox only takes its wake lock for AUDIBLE video, so step F needs an audio track and
# autoplay allowed (a policy, so no profile has to exist yet).
[ -f /tmp/av.webm ] || ffmpeg -y -hide_banner -loglevel error -f lavfi \
  -i testsrc2=size=640x360:rate=30:duration=300 -f lavfi -i sine=frequency=440:duration=300 \
  -c:v libvpx-vp9 -deadline realtime -cpu-used 8 -c:a libopus -shortest /tmp/av.webm
sudo mkdir -p /etc/firefox/policies
echo '{"policies":{"Permissions":{"Autoplay":{"Default":"allow-audio-video"}}}}' \
  | sudo tee /etc/firefox/policies/policies.json >/dev/null
cat > /tmp/fdo-inhibit.py <<'EOF'
import sys, time
from gi.repository import Gio, GLib
bus = Gio.bus_get_sync(Gio.BusType.SESSION)
r = bus.call_sync("org.freedesktop.ScreenSaver", "/org/freedesktop/ScreenSaver",
    "org.freedesktop.ScreenSaver", "Inhibit", GLib.Variant("(ss)", ("idle-test", "testing")),
    None, 0, -1, None)
print("fdo cookie", r.unpack()[0], flush=True)
time.sleep(int(sys.argv[1]))
EOF

mark "A: nothing inhibits; idle 15 s"
sleep 15; reports

mark "B: mpv starts while the session is already idle; 35 s"
run mpv $([ "$LAUNCH" = kde ] && echo --vo=wlshm) --loop=inf --no-audio /tmp/clip.webm
sleep 35; reports
[ "$LAUNCH" = sway ] && swaymsg -t get_tree | grep -o '"inhibit_idle": [a-z]*' | sort | uniq -c

mark "C: mpv quits while idle; 35 s"
pkill -x mpv; sleep 35; reports

mark "D: org.freedesktop.ScreenSaver.Inhibit held 20 s"
echo "   owner: $(busctl --user status org.freedesktop.ScreenSaver 2>&1 | grep -E '^(Comm|PID)=' | tr '\n' ' ')"
python3 /tmp/fdo-inhibit.py 20 2>&1 | sed 's/^/   /' &
sleep 18; reports; wait

mark "E: after the fdo inhibitor is released; 15 s"
sleep 15; reports

mark "F: Firefox plays the video; 25 s"
run firefox --new-window file:///tmp/av.webm
sleep 25; reports
[ "$LAUNCH" = sway ] && swaymsg -t get_tree | grep -o '"inhibit_idle": [a-z]*' | sort | uniq -c

mark "G: Firefox quits; 15 s"
pkill -x firefox-bin; pkill -x firefox; sleep 15; reports
