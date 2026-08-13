#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# Build + run the ID-register write probe. Needs com.apple.security.hypervisor,
# so it is codesigned exactly like the worker.
set -euo pipefail
cd "$(dirname "$0")"
ROOT="$(cd ../.. && pwd)"
cc -O0 -g -o idreg-write-probe idreg-write-probe.c -framework Hypervisor
codesign --sign - --force --entitlements "$ROOT/spikes/balloon-madvise/hv.entitlements" idreg-write-probe
exec ./idreg-write-probe
