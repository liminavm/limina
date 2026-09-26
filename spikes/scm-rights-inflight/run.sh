#!/bin/sh
# Build the probe and run the arms RESULTS.md reports.
set -e
cd "$(dirname "$0")"
mkdir -p build
clang -O1 -o build/inflight inflight.c
for arm in "close 0" "close 300" "keep 300" "close 300 dgram" "keep 300 dgram"; do
  # shellcheck disable=SC2086
  build/inflight $arm
done
