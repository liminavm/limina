#!/bin/bash
# Build vkstill IN THE GUEST. The L2 landmark test scp's this directory over and runs this;
# nothing is added to the guest image and no payload delivery is involved.
#
# xdg-shell's client glue is generated here rather than committed, from the `xdg-shell.xml`
# beside this script (wayland-protocols 1.41, stable/xdg-shell — MIT, unchanged since the
# protocol went stable). The XML is vendored because the guest image ships wayland-scanner but
# NOT wayland-protocols-devel, and installing it would mean mutating the canonical test image
# to run a test.
set -euo pipefail
cd "$(dirname "$0")"
wayland-scanner client-header xdg-shell.xml xdg-shell-client-protocol.h
wayland-scanner private-code   xdg-shell.xml xdg-shell-protocol.c
cc -O2 -o vkstill vkstill.c xdg-shell-protocol.c \
   $(pkg-config --cflags --libs wayland-client) -lvulkan
echo "vkstill built at $(pwd)/vkstill"
