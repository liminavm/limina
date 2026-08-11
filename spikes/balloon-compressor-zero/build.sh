#!/bin/sh
set -eu
cd "$(dirname "$0")"
clang -O2 -Wall -Wextra -o probe probe.c
echo built: "$(pwd)/probe"
