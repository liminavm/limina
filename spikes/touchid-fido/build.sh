#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build + codesign the Spike A probe (touchid-probe).
#
# Identity discovery mirrors scripts/build-app.sh: prefer a real Apple
# Development identity (SEP/keychain behavior with ad-hoc signing is NOT
# representative of the shipped app), probe that it's usable from this shell
# (keychain signing is context-dependent — see build-app.sh), fall back to
# ad-hoc with a loud warning. LIMINA_SIGN_IDENTITY overrides; "-" forces ad-hoc.
set -euo pipefail
cd "$(dirname "$0")"

swiftc -O -o touchid-probe probe.swift

SIGN_ID="${LIMINA_SIGN_IDENTITY:-}"
if [ -z "$SIGN_ID" ]; then
  SIGN_ID="$(security find-identity -v -p codesigning 2>/dev/null \
    | awk -F'"' '/Apple Development/ {print $2; exit}')"
fi
if [ -n "$SIGN_ID" ] && [ "$SIGN_ID" != "-" ]; then
  if ! codesign -s "$SIGN_ID" --force touchid-probe 2>/dev/null; then
    echo "==> WARNING: identity '$SIGN_ID' unusable from this shell; ad-hoc fallback" >&2
    echo "    (SEP/keychain results will NOT be representative — rerun from a terminal)" >&2
    SIGN_ID="-"
  fi
fi
if [ -z "$SIGN_ID" ]; then
  echo "==> WARNING: no Apple Development identity found; signing ad-hoc" >&2
  echo "    (SEP/keychain results will NOT be representative)" >&2
  SIGN_ID="-"
fi
if [ "$SIGN_ID" = "-" ]; then
  codesign -s - --force touchid-probe
fi

echo "==> signed as:"
codesign -dvv touchid-probe 2>&1 | grep -E '^(Authority|TeamIdentifier|Signature)' | sed 's/^/    /'
echo "==> built: $(pwd)/touchid-probe (run it from a seated session; expect 2 Touch ID prompts)"
