#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build + codesign the metal-heap-addressing probe (the `hv` section needs the
# hypervisor entitlement; run it with the Bash sandbox off). Usage: ./build.sh [probe args...]
set -e
cd "$(dirname "$0")"
clang -O1 -g -Wall -Wextra -fobjc-arc -o probe probe.m \
    -framework Hypervisor -framework Metal -framework Foundation
codesign --entitlements ../userptr-remap-metal/hv.entitlements -s - --force probe
echo "==> probe built + signed"
[ "$#" -eq 0 ] || ./probe "$@"
