#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Ad-hoc codesign the limina-vmm binary with the hypervisor entitlement.
# Required for hv_vm_create — without it the VMM fails with Error::VmCreate.
# Run after `cargo build -p limina-vmm`. Usage: ./sign.sh [debug|release]
set -euo pipefail
cd "$(dirname "$0")"

PROFILE="${1:-debug}"
BIN="$(cd ../.. && pwd)/target/${PROFILE}/limina-vmm"

[ -x "$BIN" ] || { echo "binary not found: $BIN (build it first)" >&2; exit 1; }

codesign --entitlements hvf-entitlements.plist -s - --force "$BIN"
codesign -d --entitlements - "$BIN" 2>&1 | grep -q hypervisor \
    && echo "signed $BIN (com.apple.security.hypervisor OK)"
