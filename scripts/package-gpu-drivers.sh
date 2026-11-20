#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
# Assemble the distro-agnostic guest GPU driver payload: the vendor drivers
# (scripts/build-gpu-drivers.sh), the virtio-gpu module as one DKMS package carrying the sources
# of every kernel series we support, and the installer (guest/gpu-drivers/install.sh).
#
# Usage: scripts/package-gpu-drivers.sh
#   SERIES  the kernel sources to vendor, as scripts/build-virtio-gpu-limina.sh takes them, one
#           per kernel series (default: Fedora 44's original v6.19.10; v7.2.9; the [linux] fork
#           at its last 7.1 rev, for the 7.1 enhanced kernels guests keep as fallbacks; and the
#           fork at its pin, whose series also serves the stock kernels of that series (Fedora 44
#           and Debian testing are on 7.3): its virtio-gpu is upstream's plus additive commits)
# Output: target/gpu-drivers/limina-gpu-drivers.tar.gz, unpacking to limina-gpu-drivers/
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
# shellcheck source=scripts/lib/manifest.sh
. scripts/lib/manifest.sh
LINUX_71=a8224c000d282a8b5b15b1a50df76d60163c70cf   # the fork's 7.1.13 tip, tag pre-rebase-2026-11-20
SERIES="${SERIES:-v6.19.10 v7.2.9 limina:$LINUX_71 limina:$(pin linux rev)}"
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
  series="$(cat "$tree/SERIES")"
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

# Without these, macOS's tar adds an AppleDouble `._*` file beside each file carrying extended
# attributes, and the attributes themselves, all of which the guest unpacks.
COPYFILE_DISABLE=1 tar --no-mac-metadata --no-xattrs -C "$OUT" -czf "$OUT/limina-gpu-drivers.tar.gz" \
  limina-gpu-drivers
echo "==> $OUT/limina-gpu-drivers.tar.gz (protocols: $(ls "$OUT/opt/limina" | tr '\n' ' ')module series: ${names[*]})"
