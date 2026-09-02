#!/bin/bash
# Host-only: touches no shared build tree (not virgl-prefix, not build-kk).
set -e
cd "$(dirname "$0")"
clang -fobjc-arc -O0 -g -o repro repro.m \
  -framework Foundation -framework Metal \
  -Wno-unguarded-availability-new
echo "built $(pwd)/repro"
