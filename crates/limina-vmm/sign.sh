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

# LIMINA_SIGN_DEBUGGABLE=1 adds com.apple.security.get-task-allow, which a debugger needs to attach
# to a signed binary. Opt-in, never the default: the entitlement lets any process on the machine
# inspect and modify a VM worker.
#
# Necessary but NOT sufficient, measured 2026-08-25: attaching Xcode to a worker that is already
# running a VM SIGKILLs it either way, with the entitlement and without. A SIGKILL writes no crash
# report and the supervisor logs no panic, so the VM simply vanishes and there is nothing to read
# afterwards -- do not spend time looking for the reason in the logs. The remaining hypothesis is
# that it is the live hv_vm_* state that cannot survive having its threads suspended, which would
# mean attaching before the VM is created is the only workable order.
PLIST=hvf-entitlements.plist
if [ "${LIMINA_SIGN_DEBUGGABLE:-0}" != "0" ]; then
    PLIST="$(mktemp -t limina-hvf-debuggable).plist"
    trap 'rm -f "$PLIST"' EXIT
    cp hvf-entitlements.plist "$PLIST"
    /usr/libexec/PlistBuddy -c \
        "Add :com.apple.security.get-task-allow bool true" "$PLIST" >/dev/null
    echo "sign.sh: LIMINA_SIGN_DEBUGGABLE — adding get-task-allow (worker is attachable)" >&2
fi

codesign --entitlements "$PLIST" -s - --force "$BIN"
codesign -d --entitlements - "$BIN" 2>&1 | grep -q hypervisor \
    && echo "signed $BIN (com.apple.security.hypervisor OK)"
