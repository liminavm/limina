#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
# Assemble the distro-agnostic guest GPU driver payload: the vendor drivers
# (scripts/build-gpu-drivers.sh), the virtio-gpu module as one DKMS package carrying the sources
# of every kernel series we support, and the installer (guest/gpu-drivers/install.sh).
#
# Usage: scripts/package-gpu-drivers.sh
#   SERIES  the kernel sources to vendor, as scripts/build-virtio-gpu-limina.sh takes them
#           (default: Fedora 44's stock v6.19.10, and the [linux] fork at its pin for the
#           enhanced tier's 7.1, whose virtio-gpu carries commits of ours)
# Output: target/gpu-drivers/limina-gpu-drivers.tar.gz, unpacking to limina-gpu-drivers/
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
# shellcheck source=scripts/lib/manifest.sh
. scripts/lib/manifest.sh
SERIES="${SERIES:-v6.19.10 limina:$(pin linux rev)}"
OUT="target/gpu-drivers"
STAGE="$OUT/limina-gpu-drivers"

[ -d "$OUT/opt/limina" ] || { echo "no vendor drivers: run scripts/build-gpu-drivers.sh" >&2; exit 1; }

rm -rf "$STAGE"
mkdir -p "$STAGE/drivers" "$STAGE/dkms/tree/series"
cp -a "$OUT/opt" "$STAGE/drivers/"
cp guest/gpu-drivers/install.sh "$STAGE/"
chmod +x "$STAGE/install.sh"

# One DKMS package, a source tree per kernel series. Its version names what it was made from, so
# a payload with other sources is another version and DKMS rebuilds.
names=()
for s in $SERIES; do
  scripts/build-virtio-gpu-limina.sh "$s" >/dev/null
  case "$s" in
    limina:*) tree="$OUT/../virtio-gpu-limina/limina-virtio-gpu-ext-limina.$(echo "${s#limina:}" | cut -c1-12)" ;;
    *) tree="$OUT/../virtio-gpu-limina/limina-virtio-gpu-ext-${s#v}.limina1" ;;
  esac
  # The series is the kernel's major.minor, which is what dkms-make.sh picks by.
  case "$s" in
    limina:*) series="$(curl -sfL "https://raw.githubusercontent.com/liminavm/linux/${s#limina:}/Makefile" |
      awk '/^VERSION =/ {v=$3} /^PATCHLEVEL =/ {p=$3} END {print v "." p}')" ;;
    *) series="$(echo "${s#v}" | cut -d. -f1-2)" ;;
  esac
  [ -n "$series" ] && [ "$series" != "." ] || { echo "cannot tell the series of $s" >&2; exit 1; }
  dst="$STAGE/dkms/tree/series/$series"
  [ ! -e "$dst" ] || { echo "two sources for series $series" >&2; exit 1; }
  mkdir -p "$dst"
  cp "$tree"/*.c "$tree"/*.h "$tree/Kbuild" "$dst/"
  names+=("$series=$s")
done
cp guest/gpu-drivers/dkms-make.sh "$STAGE/dkms/tree/"
version="$(git rev-parse --short=12 HEAD)$(git diff --quiet HEAD -- guest/virtio-gpu-limina || echo .dirty)"
sed "s/@VERSION@/$version/" guest/gpu-drivers/dkms.conf.in > "$STAGE/dkms/tree/dkms.conf"
echo "$version" > "$STAGE/dkms/VERSION"
printf '%s\n' "${names[@]}" > "$STAGE/dkms/SERIES"

tar -C "$OUT" -czf "$OUT/limina-gpu-drivers.tar.gz" limina-gpu-drivers
echo "==> $OUT/limina-gpu-drivers.tar.gz (protocols: $(ls "$OUT/opt/limina" | tr '\n' ' ')module series: ${names[*]})"
