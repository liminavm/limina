#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build, codesign with the hypervisor entitlement, and run the reclaim matrix.
set -e
cd "$(dirname "$0")"

clang -O2 -Wall -o footprint footprint.c -framework Hypervisor
codesign --entitlements hv.entitlements -s - --force footprint
echo "signed; entitlements:"; codesign -d --entitlements - footprint 2>/dev/null | tail -n +1 || true
echo

# Controls (no HVF) then the real cases (mapped via hv_vm_map).
for spec in \
    "reusable 0 0" \
    "dontneed 0 0" \
    "free 0 0" \
    "reusable 1 0" \
    "dontneed 1 0" \
    "free 1 0" \
    "reusable 1 1" \
; do
    ./footprint $spec
    echo
done
