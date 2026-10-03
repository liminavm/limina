#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# Firefox playing an AUDIBLE video: which inhibit route does it take, and does the helper see it?
# Usage: ff-audible.sh <sway|kde>
set -u
export XDG_RUNTIME_DIR=/run/user/$(id -u)
export DBUS_SESSION_BUS_ADDRESS=unix:path=$XDG_RUNTIME_DIR/bus
WAYLAND_DISPLAY=$(cd $XDG_RUNTIME_DIR && ls wayland-? | head -1); export WAYLAND_DISPLAY
[ "$1" = sway ] && export SWAYSOCK=$(ls $XDG_RUNTIME_DIR/sway-ipc.*.sock | head -1)

[ -f /tmp/av.webm ] || ffmpeg -y -hide_banner -loglevel error -f lavfi \
  -i testsrc2=size=640x360:rate=30:duration=300 -f lavfi -i sine=frequency=440:duration=300 \
  -c:v libvpx-vp9 -deadline realtime -cpu-used 8 -c:a libopus -shortest /tmp/av.webm
sudo mkdir -p /etc/firefox/policies
echo '{"policies":{"Permissions":{"Autoplay":{"Default":"allow-audio-video"}}}}' \
  | sudo tee /etc/firefox/policies/policies.json >/dev/null

pkill -x firefox-bin; pkill -x firefox; sleep 3
date +%s > /tmp/m0
nohup timeout 40 dbus-monitor --session > /tmp/mon.txt 2>&1 < /dev/null &
sleep 1
if [ "$1" = sway ]; then swaymsg -q exec "firefox --new-window file:///tmp/av.webm"
else systemd-run --user -q --collect -E WAYLAND_DISPLAY=$WAYLAND_DISPLAY firefox --new-window file:///tmp/av.webm
fi
sleep 20
grim /tmp/shot.png 2>/dev/null || spectacle -b -n -f -o /tmp/shot.png 2>/dev/null
[ "$1" = sway ] && swaymsg -t get_tree | grep -o '"inhibit_idle": [a-z]*'
echo "-- helper"; journalctl --user -u limina-agent-session --no-pager -o short-unix --since @$(cat /tmp/m0) | grep -E "telling|idle inhibitors"
echo "-- bus"; grep -A6 "member=Inhibit\b\|member=Inhibit$" /tmp/mon.txt | grep -E "member=|string|uint32|error"
echo "-- sink inputs"; pactl list sink-inputs short
