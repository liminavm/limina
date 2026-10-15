#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build + ad-hoc sign the pending-interrupt probe, then run it both ways. Needs the Bash sandbox
# off (hv_vm_*). `build.sh build-only` stops after signing, for copying to another host.
set -e
cd "$(dirname "$0")"
clang -O1 -Wall -Wextra -o probe probe.c -framework Hypervisor
codesign --entitlements ../m9-hvf-state-roundtrip/hv.entitlements -s - --force probe
[ "${1:-}" = "build-only" ] && exit 0
sw_vers -productVersion
echo "--- in-kernel GIC"; ./probe gic
echo "--- no GIC"; ./probe nogic
