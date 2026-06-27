#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build + run the libusb USB-claim probe. Usage: run.sh [VID PID]  (hex; default 1209 beee)
set -euo pipefail
cd "$(dirname "$0")"
CFLAGS=$(pkg-config --cflags libusb-1.0)
LIBS=$(pkg-config --libs libusb-1.0)
clang -O0 -g -Wall $CFLAGS probe.c $LIBS -o probe
exec ./probe "$@"
