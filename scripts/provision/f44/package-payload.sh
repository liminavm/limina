#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Package an already-assembled enhanced-tier payload into a SHIPPABLE tarball with SOURCE RPMs.
# Run in the build guest AFTER build-all.sh has produced $PAYLOAD (~/limina-guest-tools):
#   - generate the PATCHED source RPMs for mesa + mutter (rpmbuild -bs on the specs build-*.sh
#     staged in ~/rpmbuild) -> $PAYLOAD/srpms/
#   - bundle the kernel SOURCE reference (config + patches + build script + tag); the kernel has
#     no rebuildable Fedora SRPM (built from stable.git + Fedora config, not a distro SRPM)
#   - tar the whole payload -> ~/limina-guest-tools-f44.tar.zst (ship to the target Mac, extract,
#     `sudo install-enhanced.sh <dir>`; install-enhanced.sh skips the debuginfo/devel/tests RPMs)
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
PAYLOAD="${PAYLOAD:-$HOME/limina-guest-tools}"
[ -f "$PAYLOAD/manifest.txt" ] || { echo "payload not ready (no $PAYLOAD/manifest.txt) — run build-all.sh first" >&2; exit 1; }
SR="$PAYLOAD/srpms"; mkdir -p "$SR"

echo "== [1/3] patched SRPMs: mesa + mutter (rpmbuild -bs) =="
for pkg in mesa mutter; do
  spec="$HOME/rpmbuild/SPECS/$pkg.spec"
  if [ ! -f "$spec" ]; then echo "  WARN: $spec missing — skipping $pkg SRPM"; continue; fi
  if rpmbuild -bs "$spec" >/tmp/$pkg-srpm.log 2>&1; then
    src=$(ls -t "$HOME"/rpmbuild/SRPMS/$pkg-*.src.rpm 2>/dev/null | head -1) || true
    [ -n "$src" ] && cp -f "$src" "$SR"/ && echo "  ok: $(basename "$src")"
  else
    echo "  WARN: rpmbuild -bs $pkg failed (see /tmp/$pkg-srpm.log); tail:"; tail -5 /tmp/$pkg-srpm.log
  fi
done

echo "== [2/3] kernel source reference bundle =="
tmpd=$(mktemp -d); kdst="$tmpd/limina-kernel-16k-source"; mkdir -p "$kdst"
KCONFIG="$HOME/limina-build/linux/.config"
[ -f "$KCONFIG" ] && cp -f "$KCONFIG" "$kdst/config" || echo "  WARN: kernel .config not found at $KCONFIG"
cp -rf "$REPO/patches/linux" "$kdst/patches-linux"
cp -f "$REPO/scripts/provision/f44/build-kernel-rpm.sh" "$kdst/"
KVER="v$(uname -r | sed -E 's/-.*$//')"
cat > "$kdst/SOURCE.txt" <<TXT
limina-kernel-16k source reference
==================================
This kernel is NOT built from a Fedora SRPM. It is the upstream STABLE kernel at a
public tag, with this guest's Fedora config + CONFIG_ARM64_16K_PAGES=y + the linux
patches here, packaged by build-kernel-rpm.sh. Fully reproducible:

  upstream:  git clone --depth 1 --branch $KVER \\
             https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git
  config:    ./config   (also shipped inside the RPM at /lib/modules/<KREL>/config)
  patches:   ./patches-linux/*.patch  (applied tolerantly; most are already upstream)
  build:     ./build-kernel-rpm.sh    (run in an F44 guest)
TXT
tar -czf "$SR/limina-kernel-16k-source.tar.gz" -C "$tmpd" limina-kernel-16k-source
rm -rf "$tmpd"
echo "  ok: limina-kernel-16k-source.tar.gz"

echo "== refresh manifest =="
{ echo; echo "srpms (saved $(date -u +%Y-%m-%dT%H:%M:%SZ)):"; ls -1 "$SR" | sed 's/^/  - /'; } >> "$PAYLOAD/manifest.txt"

echo "== [3/3] tar the payload =="
OUT="$HOME/limina-guest-tools-f44.tar.zst"
rm -f "$OUT"
tar -C "$HOME" --owner=0 --group=0 -caf "$OUT" "$(basename "$PAYLOAD")"
echo; echo "===== manifest ====="; cat "$PAYLOAD/manifest.txt"
echo "===== tarball ====="; ls -lh "$OUT"
echo "DONE -> $OUT"
