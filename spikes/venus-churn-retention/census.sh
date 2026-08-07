#!/bin/bash
# Host-side census for the churn-retention question: how much does the worker hold, and
# how many regions of exactly the churned size?
#
# `owned unmapped` is the row that moves for GPU-class allocations (Mach memory entries /
# IOSurfaces held by reference) — RSS moves the WRONG WAY here, see
# spikes/wallpaper-backdrop-leak/RESULTS.md.
#
# Usage: census.sh <worker-pid> [label]
set -u
PID=$1
LABEL=${2:-}
SUM=$(vmmap -summary "$PID" 2>/dev/null)
PHYS=$(echo "$SUM" | awk '/^Physical footprint:/ {print $3; exit}')
REG=$(echo "$SUM" | awk '/^TOTAL / {print $NF; exit}')
OUSZ=$(echo "$SUM" | awk '/^owned unmapped  / {print $3; exit}')
OUREG=$(echo "$SUM" | awk '/^owned unmapped  / {print $NF; exit}')
# Count regions at the churned size (31.6M = 3840x2160x4). vmmap's detailed listing groups
# identical sizes; this counts them the way the original investigation did.
BIG=$(vmmap "$PID" 2>/dev/null | grep -c "31\.6M" || true)
echo "$LABEL phys=$PHYS regions=$REG owned_unmapped=$OUSZ/$OUREG at_31.6M=$BIG"
