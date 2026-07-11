#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Package an already-assembled enhanced-tier payload into a SHIPPABLE tarball with SOURCE RPMs.
# Run in the build guest AFTER build-all.sh has produced $PAYLOAD (~/limina-guest-tools):
#   - generate the PATCHED source RPM for mesa (rpmbuild -bs on the spec build-mesa-rpm.sh
#     staged in ~/rpmbuild) -> $PAYLOAD/srpms/  (mutter left the payload 2026-07-11: the GNOME
#     clipboard tier is the clipboard@limina shell extension now, stock mutter stays stock)
#   - bundle the kernel SOURCE reference (config + patches + build script + tag); the kernel has
#     no rebuildable Fedora SRPM (built from stable.git + Fedora config, not a distro SRPM)
#   - build $PAYLOAD/repo: a createrepo_c'd local dnf repo with EVERY mesa subpackage
#     Fedora ships (devel/tests included; debuginfo excluded). install-enhanced.sh installs it
#     into the guest so `dnf install mesa-libgbm-devel` etc. resolves against OUR versionlocked
#     NEVRA — Fedora's own -devel subpackages require the exact stock NEVRA the lock excludes.
#   - tar the whole payload -> ~/limina-guest-tools-f44.tar.zst (ship to the target Mac, extract,
#     `sudo install-enhanced.sh <dir>`; the installer INSTALLS only the runtime RPMs and serves
#     the rest through the repo)
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
PAYLOAD="${PAYLOAD:-$HOME/limina-guest-tools}"
[ -f "$PAYLOAD/manifest.txt" ] || { echo "payload not ready (no $PAYLOAD/manifest.txt) — run build-all.sh first" >&2; exit 1; }
SR="$PAYLOAD/srpms"; mkdir -p "$SR"

echo "== [1/4] patched SRPMs: mesa (rpmbuild -bs) =="
for pkg in mesa; do
  spec="$HOME/rpmbuild/SPECS/$pkg.spec"
  if [ ! -f "$spec" ]; then echo "  WARN: $spec missing — skipping $pkg SRPM"; continue; fi
  if rpmbuild -bs "$spec" >/tmp/$pkg-srpm.log 2>&1; then
    src=$(ls -t "$HOME"/rpmbuild/SRPMS/$pkg-*.src.rpm 2>/dev/null | head -1) || true
    [ -n "$src" ] && cp -f "$src" "$SR"/ && echo "  ok: $(basename "$src")"
  else
    echo "  WARN: rpmbuild -bs $pkg failed (see /tmp/$pkg-srpm.log); tail:"; tail -5 /tmp/$pkg-srpm.log
  fi
done

echo "== [2/4] kernel source reference bundle =="
tmpd=$(mktemp -d); kdst="$tmpd/limina-kernel-16k-source"; mkdir -p "$kdst"
KCONFIG="$HOME/limina-build/linux/.config"
[ -f "$KCONFIG" ] && cp -f "$KCONFIG" "$kdst/config" || echo "  WARN: kernel .config not found at $KCONFIG"
cp -rf "$REPO/patches/linux" "$kdst/patches-linux"
cp -f "$REPO/scripts/provision/f44/build-kernel-rpm.sh" "$kdst/"
# The kernel may be built from a DIFFERENT tag than this build guest's running kernel (e.g. a
# 7.1.2 enhanced kernel built on a 6.19.10 build guest) — honor an explicit KVER so the source
# reference matches what was actually built.
KVER="${KVER:-v$(uname -r | sed -E 's/-.*$//')}"
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

echo "== [3/4] local dnf repo: every mesa subpackage (devel/tests in, debuginfo out) =="
# The guest-side counterpart lives in install-enhanced.sh (step 3b): it copies this dir to
# /usr/share/limina-guest-tools/repo + drops a .repo file. Hardlink into the subdir so the
# tarball doesn't double in size (GNU tar stores hardlinks once).
REPODIR="$PAYLOAD/repo"
rm -rf "$REPODIR"; mkdir -p "$REPODIR"
NREPO=0
for f in "$PAYLOAD"/mesa-*.rpm; do
  [ -f "$f" ] || continue
  case "$(basename "$f")" in *debuginfo*|*debugsource*|*.src.rpm) continue ;; esac
  ln -f "$f" "$REPODIR/" 2>/dev/null || cp -f "$f" "$REPODIR/"
  NREPO=$((NREPO+1))
done
if [ "$NREPO" = 0 ]; then
  echo "  WARN: no mesa RPMs at the payload top level — repo skipped (devel stays uninstallable)"
  rm -rf "$REPODIR"
else
  # The mesa build produces the -devel subpackages (rpmbuild -bb builds them ALL); their absence
  # means the payload was hand-curated from a stale/pruned RPM dir — the exact drift that shipped
  # the devel-less -2/-3 deliveries (dnf install mesa-libgbm-devel unresolvable on the guest).
  ls "$REPODIR"/mesa-*-devel-*.rpm >/dev/null 2>&1 \
    || echo "  WARN: no mesa -devel subpackages in the payload — re-collect from ~/rpmbuild/RPMS (build-mesa-rpm.sh [5/5])"
  command -v createrepo_c >/dev/null || sudo dnf install -y createrepo_c >/dev/null
  createrepo_c --quiet "$REPODIR"
  echo "  ok: repo with $NREPO RPMs ($(ls "$REPODIR" | grep -c -- '-devel-' || true) devel) -> $REPODIR"
fi

echo "== refresh manifest =="
{ echo; echo "srpms (saved $(date -u +%Y-%m-%dT%H:%M:%SZ)):"; ls -1 "$SR" | sed 's/^/  - /'
  if [ -d "$REPODIR" ]; then
    echo "repo: $NREPO RPMs (all mesa subpackages incl. -devel; served in-guest via /etc/yum.repos.d/limina-guest-tools.repo)"
  fi
} >> "$PAYLOAD/manifest.txt"

echo "== [4/4] tar the payload =="
OUT="$HOME/limina-guest-tools-f44.tar.zst"
rm -f "$OUT"
tar -C "$HOME" --owner=0 --group=0 -caf "$OUT" "$(basename "$PAYLOAD")"
echo; echo "===== manifest ====="; cat "$PAYLOAD/manifest.txt"
echo "===== tarball ====="; ls -lh "$OUT"
echo "DONE -> $OUT"
