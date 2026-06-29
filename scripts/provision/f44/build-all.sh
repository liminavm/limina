#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build the whole enhanced tier IN-GUEST on Fedora 44, then assemble a payload directory that
# scripts/provision/install-enhanced.sh consumes. Run inside a booted basic F44 dogfood guest.
#
# Builds: kernel-16k, mesa (venus/zink), mutter, and the native limina-agent; then collects the
# RPMs + agent (+ unit + gschema override) + install-enhanced.sh + a manifest into $PAYLOAD.
#
# Usage (in the guest):  scripts/provision/f44/build-all.sh
# Then:                  sudo scripts/provision/install-enhanced.sh "$PAYLOAD" && sudo reboot
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
PAYLOAD="${PAYLOAD:-$HOME/limina-guest-tools}"
KOUT="$HOME/limina-build/kernel"; MOUT="$HOME/limina-build/mesa"; UOUT="$HOME/limina-build/mutter"

echo "############ kernel ############"; OUT="$KOUT" "$HERE/build-kernel-rpm.sh"
echo "############ mesa   ############"; OUT="$MOUT" "$HERE/build-mesa-rpm.sh"
echo "############ mutter ############"; OUT="$UOUT" "$HERE/build-mutter-rpm.sh"

echo "############ limina-agent (native) ############"
AGENT_OK=
if ! command -v cargo >/dev/null; then sudo dnf install -y cargo rust || true; fi
if command -v cargo >/dev/null; then
  ( cd "$REPO/guest" && cargo build --release -p limina-agent )
  AGENT_BIN="$REPO/guest/target/release/limina-agent"
  [ -x "$AGENT_BIN" ] && AGENT_OK=1
fi
[ -n "$AGENT_OK" ] || echo "WARN: limina-agent not built (no cargo) — install-enhanced.sh will skip it"

echo "############ assemble payload -> $PAYLOAD ############"
rm -rf "$PAYLOAD"; mkdir -p "$PAYLOAD"
# RPMs (install-enhanced.sh filters debuginfo/devel/tests at install time).
cp -f "$KOUT"/*.rpm "$MOUT"/*.rpm "$UOUT"/*.rpm "$PAYLOAD"/ 2>/dev/null || true
# The installer itself (it defaults its payload dir to its own location).
cp -f "$REPO/scripts/provision/install-enhanced.sh" "$PAYLOAD"/
# The agent + its unit + the flat-pointer gschema override (install-enhanced.sh installs these).
if [ -n "$AGENT_OK" ]; then
  cp -f "$REPO/guest/target/release/limina-agent" "$PAYLOAD"/
  cp -f "$REPO/guest/limina-agent/limina-agent.service" "$PAYLOAD"/ 2>/dev/null || true
  cp -f "$REPO/guest/limina-config/90-limina-pointer.gschema.override" "$PAYLOAD"/ 2>/dev/null || true
fi
# Manifest: the target release + the component versions, so the payload is self-describing
# (and a future install-enhanced.sh version check can read it).
{
  echo "# limina enhanced-tier guest-tools payload"
  echo "built_on: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  . /etc/os-release; echo "target: $ID $VERSION_ID  (kernel $(uname -r))"
  echo "rpms:"; ls -1 "$PAYLOAD"/*.rpm 2>/dev/null | sed 's#.*/#  - #'
  [ -n "$AGENT_OK" ] && echo "agent: limina-agent (native $(uname -m))"
} > "$PAYLOAD/manifest.txt"
cat "$PAYLOAD/manifest.txt"

echo
echo "==> payload ready: $PAYLOAD"
echo "==> install it:    sudo scripts/provision/install-enhanced.sh '$PAYLOAD' && sudo reboot"
