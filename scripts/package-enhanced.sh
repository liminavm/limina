#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
# Assemble the enhanced-tier payload install-enhanced.sh runs from: the 16 KiB kernel RPM and its
# -devel, the GPU drivers (vendor drivers + the DKMS virtio-gpu module, as
# scripts/package-gpu-drivers.sh stages them), the two agents, and the installer itself.
#
# Usage: scripts/package-enhanced.sh <rNN>
#   KERNEL_DIR  the kernel build's output: exactly one limina-kernel-16k RPM, its -devel and
#               limina-kernel-16k-source.tar.gz (default target/enhanced-rpms/kernel, which
#               `cargo xtask enhanced` writes)
#   NOTE        a line for the manifest saying what changed since the previous revision
#   PAYLOAD_DIR where the tarball goes (default payload/, gitignored)
# Output: $PAYLOAD_DIR/limina-guest-tools-f44-<rNN>.tar.zst, unpacking to limina-guest-tools/
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
REV="${1:?usage: $0 <rNN>}"
case "$REV" in r[0-9]*) ;; *) echo "revision is rNN, not $REV" >&2; exit 1 ;; esac
KERNEL_DIR="${KERNEL_DIR:-target/enhanced-rpms/kernel}"
GPU="target/gpu-drivers/limina-gpu-drivers"
PAYLOAD_DIR="${PAYLOAD_DIR:-payload}"
OUT="$PAYLOAD_DIR/limina-guest-tools-f44-$REV.tar.zst"
[ ! -e "$OUT" ] || { echo "$OUT exists: a revision is never rebuilt in place" >&2; exit 1; }

kernels=()
for r in "$KERNEL_DIR"/limina-kernel-16k-*.rpm; do
  case "$r" in *-devel-*) ;; *) [ -e "$r" ] && kernels+=("$r") ;; esac
done
[ "${#kernels[@]}" = 1 ] || { echo "$KERNEL_DIR holds ${#kernels[@]} kernel RPMs, not one" >&2; exit 1; }
KRPM="${kernels[0]}"
DEVEL="$KERNEL_DIR/$(basename "$KRPM" | sed 's/^limina-kernel-16k-/limina-kernel-16k-devel-/')"
KSRC="$KERNEL_DIR/limina-kernel-16k-source.tar.gz"
for f in "$DEVEL" "$KSRC"; do [ -f "$f" ] || { echo "missing $f" >&2; exit 1; }; done
[ -x "$GPU/install.sh" ] || { echo "no $GPU: run scripts/package-gpu-drivers.sh" >&2; exit 1; }

echo "==> agents (aarch64-unknown-linux-musl, static)"
(cd guest && cargo build --release -p limina-agent -p limina-agent-session)
AGENTS=guest/target/aarch64-unknown-linux-musl/release

STAGE="target/enhanced-payload/limina-guest-tools"
rm -rf target/enhanced-payload
mkdir -p "$STAGE/srpms"
cp "$KRPM" "$DEVEL" "$STAGE/"
cp "$KSRC" "$STAGE/srpms/"
cp -a "$GPU" "$STAGE/limina-gpu-drivers"
cp "$AGENTS/limina-agent" "$AGENTS/limina-agent-session" "$STAGE/"
cp guest/limina-agent/limina-agent.service \
   guest/limina-agent-session/limina-agent-session.service "$STAGE/"
cp scripts/provision/install-enhanced.sh "$STAGE/"
chmod +x "$STAGE/install-enhanced.sh"

sha() { shasum -a 256 "$1" | awk '{print $1}'; }
dirty() { git diff --quiet HEAD -- "$@" || echo " (dirty)"; }
drivers="$(ls "$GPU/drivers/opt/limina" | tr '\n' ' ')"
{
  echo "# limina enhanced-tier guest-tools payload"
  echo "revision: $REV"
  [ -z "${NOTE:-}" ] || echo "  $NOTE"
  echo "built_on: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "limina: $(git rev-parse --short=12 HEAD)$(dirty scripts guest)"
  echo "target: fedora 44"
  echo
  echo "kernel:"
  echo "  - $(basename "$KRPM")"
  echo "  - $(basename "$DEVEL")"
  echo
  echo "gpu drivers:"
  for w in $drivers; do
    echo "  - /opt/limina/$w  mesa $(cat "$GPU/drivers/opt/limina/$w/MESA_REV")"
  done
  echo "  - dkms limina-virtio-gpu $(cat "$GPU/dkms/VERSION")  series: $(tr '\n' ' ' < "$GPU/dkms/SERIES")"
  echo
  echo "agents:"
  echo "  - limina-agent  sha256=$(sha "$STAGE/limina-agent")"
  echo "  - limina-agent-session  sha256=$(sha "$STAGE/limina-agent-session")"
} > "$STAGE/manifest.txt"

mkdir -p "$PAYLOAD_DIR"
COPYFILE_DISABLE=1 tar --no-mac-metadata --no-xattrs -C target/enhanced-payload --zstd -cf "$OUT" \
  limina-guest-tools
echo "==> $OUT"
cat "$STAGE/manifest.txt"
