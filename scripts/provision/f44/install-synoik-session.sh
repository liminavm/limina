#!/usr/bin/env bash
# Build synoik in-guest and make it the session compositor.
#
# Run this INSIDE a booted enhanced-tier Fedora guest (see this directory's README for why
# in-guest rather than a macOS container build). It is the reproducible half of
# `Fedora-Workstation-44.enhanced.synoik.raw` — see docs/images.md for the host-side steps
# (clone the base image, boot it with --net) that bracket this script.
#
# Why the image exists at all: a whole class of host bugs is only reachable when the
# COMPOSITOR imports client dmabufs through Vulkan/venus. Mutter composites with GL, so under
# mutter those paths are never exercised and a "cannot reproduce" is a FALSE NEGATIVE, not a
# refutation. The vrend/KK stride shear (spikes/vrend-stride-2026-08-14) is the worked example.
#
# Idempotent: re-running updates the checkout, rebuilds, and reinstalls the session.
#
# Env knobs:
#   SYNOIK_REF=main             git ref to build
#   SYNOIK_REPO=https://github.com/kov/synoik
#   TEST_USER=$SUDO_USER/$USER  the account whose GNOME session is replaced
#   PROFILE=release             which build the session runs
#
# Usage (as the target user, with passwordless sudo):
#   scripts/provision/f44/install-synoik-session.sh
set -euo pipefail

SYNOIK_REPO="${SYNOIK_REPO:-https://github.com/kov/synoik}"
SYNOIK_REF="${SYNOIK_REF:-main}"
PROFILE="${PROFILE:-release}"
# Deliberately NOT root: the build must run as the user who will own the checkout, because the
# session unit points straight at $HOME/synoik/target/$PROFILE/synoik.
TEST_USER="${TEST_USER:-${SUDO_USER:-$USER}}"
if [ "$(id -u)" -eq 0 ]; then
    echo "error: run as the target user (it sudo's where it needs to), not as root" >&2
    exit 1
fi
home="$(getent passwd "$TEST_USER" | cut -d: -f6)"
checkout="$home/synoik"

echo "==> deps"
# synoik's BuildRequires, plus the toolchain. `dnf builddep` is not used: the spec is an
# .rpkg template whose macros don't expand outside an rpkg checkout, so the list is mirrored
# here. Keep in sync with synoik.spec.rpkg.
#
# glslang is the one dep NOT in the spec: synoik-vk/build.rs shells out to glslangValidator
# and panics with a bare NotFound if it is missing. Reported upstream; until the spec carries
# it, a builddep-only install fails here and the error names build.rs rather than the package.
sudo dnf install -y --setopt=install_weak_deps=False \
    git cargo rustc clang glslang \
    cargo-rpm-macros systemd-devel wayland-devel pipewire-devel pango-devel \
    glib2-devel cairo-gobject-devel mesa-libEGL mesa-libEGL-devel \
    libudev-devel mesa-libgbm-devel libxkbcommon-devel libinput-devel \
    dbus-devel libseat-devel libdisplay-info-devel \
    xwayland-satellite grim >/dev/null

echo "==> checkout ($SYNOIK_REF)"
if [ -d "$checkout/.git" ]; then
    git -C "$checkout" fetch --quiet origin
    git -C "$checkout" checkout --quiet "$SYNOIK_REF"
    git -C "$checkout" pull --quiet --ff-only || true
else
    git clone --quiet "$SYNOIK_REPO" "$checkout"
    git -C "$checkout" checkout --quiet "$SYNOIK_REF"
fi
echo "    $(git -C "$checkout" log --oneline -1)"

echo "==> build ($PROFILE)"
cd "$checkout"
if [ "$PROFILE" = "release" ]; then cargo build --release; else cargo build; fi

echo "==> install session"
# synoik's own installer is the source of truth for the unit contents — it writes the
# org.gnome.Shell@user.service drop-in and compiles synoik's GSettings schemas into a PRIVATE
# schema dir (two files declaring one schema id in a shared dir makes glib-compile-schemas
# silently drop one and still exit 0, and the real gnome-shell is installed alongside here).
# Don't hand-roll the drop-in; call the installer so it stays correct as synoik changes.
sudo TEST_USER="$TEST_USER" PROFILE="$PROFILE" scripts/install-test-session.sh

cat <<EOF

==> done. Reboot the guest (not just a GDM restart) and the normal GNOME session comes up on
    synoik. GDM autologin for '$TEST_USER' must be enabled for it to come up unattended.

    Two traps, each of which has cost a run:

    - Restarting GDM does NOT re-read /etc/environment.d — the systemd *user manager*
      survives it. Reboot, and verify the driver env at /proc/\$(pgrep synoik)/environ
      rather than by reading the file.
    - synoik's Wayland socket is wayland-1, NOT wayland-0 (gdm holds -0). A client launched
      with a hardcoded WAYLAND_DISPLAY=wayland-0 never connects, so nothing is imported and
      every measurement reads clean — a false negative across a whole sweep. Discover it:
          WAYLAND_DISPLAY=\$(basename \$(ls /run/user/1000/wayland-* | grep -v lock | head -1))

    Iterating on synoik afterwards: rebuild in-guest and log out/in. The unit always runs
    whatever is at target/$PROFILE/synoik — no reinstall step.
EOF
