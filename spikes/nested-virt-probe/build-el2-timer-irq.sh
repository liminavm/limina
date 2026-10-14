#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build, sign and run el2-timer-irq (which timer interrupts reach a vCPU at EL2). Needs an M3 or
# later Mac and no tool sandbox (hv_vm_*). Pass `build-only` to skip the run.
set -e
cd "$(dirname "$0")"
clang -O1 -g -Wall -Wextra -o el2-timer-irq el2-timer-irq.c -framework Hypervisor
codesign --entitlements ../m9-hvf-state-roundtrip/hv.entitlements -s - --force el2-timer-irq
if [ "${1:-}" != "build-only" ]; then
    ./el2-timer-irq
fi
