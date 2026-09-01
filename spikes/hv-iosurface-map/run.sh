#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build, codesign with the hypervisor entitlement, and run. hv_vm_create fails
# with HV_DENIED without the entitlement, which is by far the likeliest cause of
# a mystery failure here.
set -e
cd "$(dirname "$0")"

clang -O2 -Wall -Wextra -o probe probe.c -framework Hypervisor -framework IOSurface \
    -framework CoreFoundation
codesign --entitlements hv.entitlements -s - --force probe

./probe
