#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Make KDE Plasma the autologin session of an enhanced guest: the reproducible half of
# `Fedora-Workstation-<REL>.enhanced.kde.raw` (docs/images.md has the host-side steps). Run IN
# THE GUEST as the autologin user, with networking; idempotent.
#
#   scripts/provision/fedora/install-kde-session.sh
#
# GDM stays the display manager and the GNOME session stays installed: AccountsService's
# `Session=plasma` is what makes autologin come up in Plasma (set it back to switch).
# Beyond Plasma it sets test conveniences, not KDE defaults:
#   - no screen autolock, no lock on resume and no display-off when idle: an idle test otherwise
#     ends at the lock screen after five minutes, and nothing can map a window behind it;
#   - Firefox may autoplay audible media, because Firefox inhibits idle only for audible video;
#   - swayidle and wayland-utils, the idle-inhibit probes (spikes/idle-inhibit-sources/).
set -euo pipefail
USER_NAME="$(id -un)"
[ "$USER_NAME" != root ] || { echo "run as the autologin user (not root); it sudos as needed" >&2; exit 1; }

echo "-- Plasma"
sudo dnf install -y plasma-desktop plasma-workspace-wayland powerdevil kwin konsole mpv \
  swayidle wayland-utils

echo "-- autologin into Plasma"
sudo install -d -m 0700 /var/lib/AccountsService/users
printf '[User]\nSession=plasma\nSystemAccount=false\n' \
  | sudo tee "/var/lib/AccountsService/users/$USER_NAME" >/dev/null

echo "-- no lock, no display-off when idle"
mkdir -p "$HOME/.config"
printf '[Daemon]\nAutolock=false\nLockOnResume=false\n' > "$HOME/.config/kscreenlockerrc"
printf '[AC][Display]\nTurnOffDisplayWhenIdle=false\n' > "$HOME/.config/powerdevilrc"

echo "-- Firefox autoplays audible media"
sudo install -d -m 0755 /etc/firefox/policies
echo '{"policies":{"Permissions":{"Autoplay":{"Default":"allow-audio-video"}}}}' \
  | sudo tee /etc/firefox/policies/policies.json >/dev/null

sudo restorecon -RF /var/lib/AccountsService/users /etc/firefox/policies "$HOME/.config"
sudo dnf clean all -q
echo "==> Plasma is the autologin session for $USER_NAME; reboot the guest to enter it."
