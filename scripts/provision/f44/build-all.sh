#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build the whole enhanced tier IN-GUEST on Fedora 44, then assemble a payload directory that
# scripts/provision/install-enhanced.sh consumes. Run inside a booted basic F44 dogfood guest.
#
# Builds: kernel-16k, mesa (venus/zink), and the native limina-agent +
# limina-agent-session; then collects the RPMs + agents (+ units + gschema override) +
# the clipboard@limina gnome-shell extension (plain files, nothing to build) +
# install-enhanced.sh + a manifest into $PAYLOAD.
#
# mutter is NOT part of the payload anymore (2026-07-11): the clipboard bridge's GNOME
# tier is the shell extension (guest/gnome-shell-extension/), so stock mutter stays
# stock — no patched rebuild to rebase, no distro update displacing it. The optional
# build-mutter-rpm.sh remains for experiments (patches/mutter/0003 ext-data-control).
#
# Usage (in the guest):  scripts/provision/f44/build-all.sh
# Then:                  sudo scripts/provision/install-enhanced.sh "$PAYLOAD" && sudo reboot
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
PAYLOAD="${PAYLOAD:-$HOME/limina-guest-tools}"
KOUT="$HOME/limina-build/kernel"; MOUT="$HOME/limina-build/mesa"

echo "############ kernel ############"; OUT="$KOUT" "$HERE/build-kernel-rpm.sh"
echo "############ mesa   ############"; OUT="$MOUT" "$HERE/build-mesa-rpm.sh"

echo "############ limina-agent + limina-agent-session (native gnu) ############"
# guest/.cargo/config.toml defaults to aarch64-unknown-linux-musl (the portable STATIC artifact the
# over-SSH install-guest-agent.sh ships). But Fedora's rust does NOT package the musl std
# (rust-std-static-*-musl is absent, no rustup in-guest), so a bare `cargo build` here dies with
# "can't find crate for `core`". We install INTO this Fedora guest, so build NATIVE gnu (links the
# guest glibc — fine for the same-distro enhanced image); the explicit --target overrides the musl
# default and reads from the matching target/<triple>/release path.
AGENT_OK=
SESSION_OK=
AGENT_TGT=aarch64-unknown-linux-gnu
if ! command -v cargo >/dev/null; then sudo dnf install -y cargo rust || true; fi
if command -v cargo >/dev/null; then
  ( cd "$REPO/guest" && cargo build --release -p limina-agent -p limina-agent-session --target "$AGENT_TGT" )
  AGENT_BIN="$REPO/guest/target/$AGENT_TGT/release/limina-agent"
  SESSION_BIN="$REPO/guest/target/$AGENT_TGT/release/limina-agent-session"
  [ -x "$AGENT_BIN" ] && AGENT_OK=1
  [ -x "$SESSION_BIN" ] && SESSION_OK=1
fi
[ -n "$AGENT_OK" ] || echo "WARN: limina-agent not built (no cargo) — install-enhanced.sh will skip it"
[ -n "$SESSION_OK" ] || echo "WARN: limina-agent-session not built — clipboard will stay inactive"

echo "############ assemble payload -> $PAYLOAD ############"
rm -rf "$PAYLOAD"; mkdir -p "$PAYLOAD"
# RPMs — ALL subpackages (devel/tests/debuginfo included; the mesa build produces the
# full set Fedora ships). install-enhanced.sh installs only the runtime ones by default and
# serves the rest via the local repo package-payload.sh builds — so do NOT hand-prune this set;
# a devel-less payload leaves e.g. mesa-libgbm-devel uninstallable on a versionlocked guest.
cp -f "$KOUT"/*.rpm "$MOUT"/*.rpm "$PAYLOAD"/ 2>/dev/null || true
# The installer itself (it defaults its payload dir to its own location).
cp -f "$REPO/scripts/provision/install-enhanced.sh" "$PAYLOAD"/
# The agents + their units + the flat-pointer gschema override (install-enhanced.sh installs these).
if [ -n "$AGENT_OK" ]; then
  cp -f "$AGENT_BIN" "$PAYLOAD"/
  cp -f "$REPO/guest/limina-agent/limina-agent.service" "$PAYLOAD"/ 2>/dev/null || true
  cp -f "$REPO/guest/limina-config/90-limina-pointer.gschema.override" "$PAYLOAD"/ 2>/dev/null || true
fi
# The per-session helper (clipboard bridge): a systemd USER unit, so it rides the payload
# separately from the system agent (each lights up on its own prerequisite).
if [ -n "$SESSION_OK" ]; then
  cp -f "$SESSION_BIN" "$PAYLOAD"/
  cp -f "$REPO/guest/limina-agent-session/limina-agent-session.service" "$PAYLOAD"/ 2>/dev/null || true
fi
# The clipboard@limina gnome-shell extension (the GNOME tier of the clipboard bridge):
# plain GJS files, no build step. install-enhanced.sh drops it under
# /usr/share/gnome-shell/extensions; limina-agent-session self-enables it per user.
cp -rf "$REPO/guest/gnome-shell-extension/clipboard@limina" "$PAYLOAD"/
# Manifest: the target release + the component versions, so the payload is self-describing
# (and a future install-enhanced.sh version check can read it).
{
  echo "# limina enhanced-tier guest-tools payload"
  echo "built_on: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  . /etc/os-release; echo "target: $ID $VERSION_ID  (kernel $(uname -r))"
  echo "rpms:"; ls -1 "$PAYLOAD"/*.rpm 2>/dev/null | sed 's#.*/#  - #'
  [ -n "$AGENT_OK" ] && echo "agent: limina-agent ($AGENT_TGT)"
  [ -n "$SESSION_OK" ] && echo "agent: limina-agent-session ($AGENT_TGT)" || true
  echo "extension: clipboard@limina (gnome-shell)"
} > "$PAYLOAD/manifest.txt"
cat "$PAYLOAD/manifest.txt"

echo
echo "==> payload ready: $PAYLOAD"
echo "==> install it:    sudo scripts/provision/install-enhanced.sh '$PAYLOAD' && sudo reboot"
