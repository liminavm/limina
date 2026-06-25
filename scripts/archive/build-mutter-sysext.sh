#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build the limina enhanced-tier patched **mutter** (49.5 + patches/mutter/*.patch) and
# assemble its full /usr install into a **systemd-sysext** — same delivery model as the mesa
# sysext (scripts/build-mesa-zink-sysext.sh). The enhanced compositor layers on top of the
# stock GNOME so the seated desktop runs correctly on venus/KK.
#
# WHY patched mutter (see patches/mutter/README.md):
#   0001  the #32 fix: cogl stencil-clip degrade when the framebuffer has no stencil buffer
#         (gnome-shell-on-venus/KK renders without one) + meta-stage-impl clipped-redraw
#         degrade. Without it the seated desktop corrupts/crashes.
#   0002  guard meta_x11_display_init_frames_client against a NULL launch (else the first
#         X11 app SIGSEGVs the whole compositor).
#   0003  ext-data-control-v1, so limina-agent manages the clipboard as a focusless Wayland
#         client instead of a RemoteDesktop session (no permanent screen-share indicator).
#
# THE INSTALL-PATH TRAP (encoded in spikes/venus-draw-probe/install-mutter-fix.sh): gnome-shell
# loads libmutter-17.so.0 from /usr/lib64/ DIRECTLY, while every other libmutter*.so lives in
# /usr/lib64/mutter-17/. A DESTDIR `ninja install` lays them out correctly on its own, and the
# sysext overlays /usr verbatim — so we ship the WHOLE install tree and the layout is right by
# construction (no hand-moves). We must build the FULL lib set (cogl + clutter + mtk + mutter)
# from ONE tree: mixing our cogl with Fedora-stock clutter is an untested ABI blend.
#
# --libexecdir=/usr/libexec is load-bearing (not cosmetic): MUTTER_LIBEXECDIR is compiled into
# libmutter and locates the X11 frames client; the meson default (/usr/local) points at nothing
# and any X11 app crashes the session (the bug 0002 also guards).
#
# FEDORA_REL MUST match the guest (mutter links the guest's GLib/GTK/wayland sonames; the
# libmutter-17 ABI must match the guest's gnome-shell). F43 ships mutter 49.x (= libmutter-17),
# so tag 49.5 is a drop-in.
#
# Output (gitignored): target/test-guest/mutter-sysext/  (rsync into /var/lib/extensions/<name>/)
set -euxo pipefail
cd "$(dirname "$0")/.."
REPO="$PWD"

MUTTER_TAG="${MUTTER_TAG:-49.5}"
MUTTER_GIT="${MUTTER_GIT:-https://gitlab.gnome.org/GNOME/mutter.git}"
FEDORA_REL="${FEDORA_REL:-43}"
EXT_NAME="limina-mutter"

OUTROOT="$REPO/target/test-guest"
EXT="$OUTROOT/mutter-sysext"
mkdir -p "$OUTROOT"
rm -rf "$EXT"

VOL="limina-mutter-build"
container volume inspect "$VOL" >/dev/null 2>&1 || container volume create "$VOL" >/dev/null

container run --rm \
  --cpus 8 --memory 8g \
  -v "$REPO/patches/mutter:/patches:ro" \
  -v "$OUTROOT:/outroot" \
  -v "$VOL:/build" \
  "fedora:$FEDORA_REL" bash -euxo pipefail -c '
    dnf -y install git meson ninja-build gcc gcc-c++ >/dev/null
    dnf -y builddep mutter >/dev/null

    cd /build
    [ -d mutter/.git ] || git clone "'"$MUTTER_GIT"'" mutter
    cd mutter
    git config --global --add safe.directory /build/mutter
    rm -f .git/shallow.lock .git/index.lock .git/*.lock 2>/dev/null || true
    git fetch --tags --depth=1 origin "'"$MUTTER_TAG"'"
    git checkout -f "'"$MUTTER_TAG"'"
    git clean -fdx -e build 2>/dev/null || true
    echo "=== mutter HEAD ==="; git log --oneline -1

    # All carried patches; a failure is FATAL (product build).
    for p in $(ls /patches/*.patch 2>/dev/null | sort); do
      echo "=== applying $(basename "$p") ==="
      if git apply --verbose "$p"; then echo "OK: $p";
      else echo "FAILED to apply $p on '"$MUTTER_TAG"'"; exit 3; fi
    done

    # Fedora layout: libdir=lib64; libexecdir=/usr/libexec (compiled into libmutter, see header).
    rm -rf build
    meson setup build \
      --prefix=/usr --libdir=lib64 --libexecdir=/usr/libexec \
      -Dtests=disabled -Dbuildtype=release
    ninja -C build
    rm -rf /tmp/stage
    DESTDIR=/tmp/stage ninja -C build install

    echo "=== mutter install footprint (sanity: libmutter-17 in lib64, rest in mutter-17/) ==="
    ls -la /tmp/stage/usr/lib64/libmutter-17.so* 2>&1 | head
    ls /tmp/stage/usr/lib64/mutter-17/ 2>/dev/null | head

    # Ship the WHOLE mutter /usr install (full cogl+clutter+mtk+mutter set, correct layout).
    EXT=/outroot/mutter-sysext
    rm -rf "$EXT"; mkdir -p "$EXT/usr/lib/extension-release.d"
    cp -a /tmp/stage/usr/. "$EXT/usr/"
    cat > "$EXT/usr/lib/extension-release.d/extension-release.'"$EXT_NAME"'" <<EOF
ID=fedora
VERSION_ID='"$FEDORA_REL"'
EOF
    echo "=== sysext assembled ==="; du -sh "$EXT"
  '
echo "==> done: $EXT (patched mutter $MUTTER_TAG, shadows stock mutter). Deliver via systemd-sysext."
