#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build, codesign with the hypervisor entitlement, and run the probe. Needs no guest and no
# vCPU: each case is a bare VM with four host pages mapped at a fixed GPA.
set -e
cd "$(dirname "$0")"
clang -O2 -Wall -o probe probe.c -framework Hypervisor
codesign --entitlements hv.entitlements -s - --force probe
./probe
