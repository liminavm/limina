#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# Does our virtual authenticator produce a credential an independent verifier accepts?
#
# Reported 2026-08-08: adding limina's FIDO device to GitHub as a passkey fails *after* Touch ID
# fires (Firefox, in the guest). Touch ID firing means makeCredential reached the enclave and
# signed, so the fault is downstream of us doing the work: either the CTAP response we build is
# malformed, or the attestation is one a relying party refuses.
#
# GitHub is a bad instrument for that question — one bit of output, a network round trip, and a
# human in the loop. `fido2-cred -V` runs the same verification an RP does, locally, and prints
# which step failed. Run it BEFORE reasoning about what GitHub might dislike.
#
# Run INSIDE the guest (needs fido2-tools + the limina FIDO device). Touch ID fires on the host,
# so a human must authenticate when prompted.
#
# Usage: bash verify-credential.sh [/dev/hidraw0] [rp-id]
set -u
DEV="${1:-/dev/hidraw0}"
RP="${2:-example.com}"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

# `base64`, not `openssl base64`: a stock Fedora guest has no openssl binary, and the empty output
# made fido2-cred fail at INPUT parsing — which the failure branch below then reported as the
# device rejecting the credential. An oracle that can blame the system under test for its own
# broken input is worse than no oracle (2026-08-08).
b64() { head -c 32 /dev/urandom | base64 | tr -d '\n'; }
CDH=$(b64)
UID_B64=$(b64)
if [ -z "$CDH" ] || [ -z "$UID_B64" ]; then
  echo "PROBE BROKEN: could not generate base64 input; nothing was asked of the authenticator."
  exit 2
fi
printf '%s\n%s\ntestuser\n%s\n' "$CDH" "$RP" "$UID_B64" > "$TMP/param"

echo "=== makeCredential (resident, uv) — touch the sensor on the HOST when prompted ==="
if ! sudo fido2-cred -M -r -v -i "$TMP/param" -o "$TMP/cred" "$DEV" es256; then
  echo "makeCredential did not complete. 'input error' means fido2-cred rejected the PARAMETERS —"
  echo "that is this script's fault, not the authenticator's. Any other error is the device."
  exit 1
fi

echo "=== verify (what a relying party does) ==="
if sudo fido2-cred -V -i "$TMP/cred" -o "$TMP/pubkey" es256; then
  echo "PASS: an independent verifier accepts this credential."
  echo "So the attestation is well-formed and the disagreement is with GitHub's POLICY, not our"
  echo "encoding — look at what it requires of a passkey (discoverability, credProps, UV) next."
else
  echo "FAIL: verification rejected it. This reproduces the GitHub failure with no GitHub in it."
fi
