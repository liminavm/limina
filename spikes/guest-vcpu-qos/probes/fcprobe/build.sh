#!/bin/bash
# Generates the protocol glue and builds fcprobe.
set -e
cd "$(dirname "$0")"
P=$(pkg-config --variable=pkgdatadir wayland-protocols)
for x in stable/xdg-shell/xdg-shell stable/presentation-time/presentation-time; do
  n=$(basename $x)
  wayland-scanner client-header $P/$x.xml $n-client-protocol.h
  wayland-scanner private-code $P/$x.xml $n-protocol.c
done
cc -O2 -Wall -o fcprobe fcprobe.c xdg-shell-protocol.c presentation-time-protocol.c $(pkg-config --cflags --libs wayland-client)
