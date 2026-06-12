#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build + install the patched mutter (the #32 stencil-clip fix) into a RUNNING guest, correctly.
#
# This script exists because a half-day was lost to the path trap it encodes:
# gnome-shell loads libmutter-17.so.0.0.0 from /usr/lib64/ DIRECTLY, while every other
# libmutter*.so lives in /usr/lib64/mutter-17/. An install that copies libmutter-17 into
# mutter-17/ leaves the file inert and the meta-stage-impl degrade fix (THE #32 mitigation)
# unloaded — cogl's zero-init still works (one-time "no stencil buffer" warning fires), which
# makes the broken install look half-alive and very confusing. Also: install the FULL lib set
# (cogl + clutter + mtk + mutter) from one build — mixing our cogl with Fedora-stock clutter is
# an untested ABI blend.
#
# Usage: ./install-mutter-fix.sh            # ships third_party/mutter, builds in guest, installs
# Prereqs: guest up on ssh -p 2222 (limina --net), builddeps in guest (sudo dnf builddep -y mutter).
# The macOS tar AppleDouble files (._*) must be removed before meson parses subprojects/*.wrap.
set -e -o pipefail
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
SSH="ssh -p 2222 -o StrictHostKeyChecking=no claude@127.0.0.1"

echo "== shipping third_party/mutter (with patches applied) to guest"
tar -C "$ROOT/third_party" -czf /tmp/mutter-fixed-src.tgz mutter
scp -P 2222 -o StrictHostKeyChecking=no /tmp/mutter-fixed-src.tgz claude@127.0.0.1:/tmp/

echo "== building in guest (meson -Dtests=disabled; takes ~7 min)"
$SSH 'cd /tmp && rm -rf mutter-src && mkdir mutter-src && tar xzf mutter-fixed-src.tgz -C mutter-src --strip-components=1 2>/dev/null;
      find mutter-src \( -name "._*" -o -name ".DS_Store" \) -delete;
      cd mutter-src && meson setup build -Dtests=disabled --prefix=/usr --libexecdir=/usr/libexec >/tmp/meson.log 2>&1 && ninja -C build >/tmp/ninja.log 2>&1'
# prefix/libexecdir matter even though we hand-copy only libs: MUTTER_LIBEXECDIR is compiled
# into libmutter and locates the X11 frames client. The default (/usr/local) pointed at
# nothing and any X11 app crashed the whole session (see patches/mutter/0002-*).

echo "== installing the FULL lib set"
$SSH 'set -e
# everything except libmutter-17 goes into /usr/lib64/mutter-17/
for f in $(find /tmp/mutter-src/build -name "libmutter*.so.0.0.0" -not -path "*.p*"); do
  b=$(basename "$f")
  if [ "$b" != "libmutter-17.so.0.0.0" ] && [ -f "/usr/lib64/mutter-17/$b" ]; then
    sudo install -m755 "$f" "/tmp/i-$b" && sudo mv "/tmp/i-$b" "/usr/lib64/mutter-17/$b" && echo "installed mutter-17/$b"
  fi
done
# libmutter-17 lives in /usr/lib64/ DIRECTLY — the path trap. Never put it in mutter-17/.
sudo rm -f /usr/lib64/mutter-17/libmutter-17.so.0.0.0
sudo install -m755 /tmp/mutter-src/build/src/libmutter-17.so.0.0.0 /tmp/i-m17
sudo mv /tmp/i-m17 /usr/lib64/libmutter-17.so.0.0.0 && echo "installed /usr/lib64/libmutter-17.so.0.0.0"'

echo "== restarting gdm"
$SSH 'sudo systemctl restart gdm'
echo "DONE. Verify: search bar present at seat; journal shows [LIMINA-CLIP] DEGRADE on multi-rect clips under stress."
echo "If booted DIRECTLY on the golden image (LIMINA_DISK), the install persists; on a clone it dies with the clone."
