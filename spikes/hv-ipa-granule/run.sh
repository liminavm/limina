#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build, codesign with the hypervisor entitlement, and run one arm per granule.
# hv_vm_create is once per process, so the granule is a per-process choice: run twice.
set -e
cd "$(dirname "$0")"

clang -O2 -Wall -o granule granule.c -framework Hypervisor
codesign --entitlements hv.entitlements -s - --force granule

for arm in 16k 4k; do
    echo "================ granule: $arm ================"
    ./granule "$arm" || echo "(exit $?)"
    echo
done
