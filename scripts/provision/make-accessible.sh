#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Promote a freshly-installed Fedora guest into the limina "accessible" BASE image — the reusable
# stock-tier base that `stock.test` snapshots from and that enhanced-tier provisioning starts on
# (see docs/images.md). Idempotent and release-agnostic (F43 / F44 / F45). Run IN THE GUEST as the
# autologin user (it sudos as needed); boot with `--net` so the dnf step has network.
#
# Precondition you do first (NOT scripted — a kickstart to skip it is a tracked follow-up):
#   gnome-initial-setup (creates the user) + your SSH pubkey in ~/.ssh/authorized_keys (so this is
#   reachable). For F44 the existing boot.raw already satisfies this.
#
# What it sets up (mirrors docs/images.md's `accessible` description):
#   - sshd enabled
#   - GDM autologin for this user (seated GNOME, no password prompt — the L2 baseline vehicle)
#   - passwordless sudo for this user
#   - vulkan-tools (vulkaninfo; Fedora Workstation omits it, the venus tests need it)
#   - a system no-idle-lock gschema override (long tests never hit the lock screen)
#   - console=tty0 console=ttyAMA0 on the GRUB args (GOP window + the PL011 serial --console captures)
#
# After it completes: verify autologin + `vulkaninfo`, then clean-poweroff and save the disk on the
# host as Fedora-Workstation-<REL>.accessible.raw.
set -euo pipefail
USER_NAME="${SUDO_USER:-$(id -un)}"
[ "$USER_NAME" != root ] || { echo "run as the autologin user (not root); it sudos as needed" >&2; exit 1; }
echo "==> promoting this guest to the limina 'accessible' base for user '$USER_NAME'"

echo "-- sshd enabled"
sudo systemctl enable --now sshd

echo "-- GDM autologin for $USER_NAME"
sudo install -d -m 0755 /etc/gdm
sudo python3 - "$USER_NAME" <<'PY'
import configparser, sys
user = sys.argv[1]; path = "/etc/gdm/custom.conf"
c = configparser.ConfigParser(); c.optionxform = str; c.read(path)
if not c.has_section("daemon"): c.add_section("daemon")
c["daemon"]["AutomaticLoginEnable"] = "True"
c["daemon"]["AutomaticLogin"] = user
with open(path, "w") as f: c.write(f)
PY

echo "-- passwordless sudo"
echo "$USER_NAME ALL=(ALL) NOPASSWD: ALL" | sudo tee /etc/sudoers.d/91-limina-nopasswd >/dev/null
sudo chmod 0440 /etc/sudoers.d/91-limina-nopasswd

echo "-- vulkan-tools (vulkaninfo)"
command -v vulkaninfo >/dev/null || sudo dnf install -y vulkan-tools

echo "-- L2 replay test tooling (glmark2 GL probe, apitrace GL replay, gfxreconstruct VK replay)"
# These back the venus_replay L2 trace-replay tests (the GL/VK rendering regression guards).
# glmark2 (its glmark2-es2 is the X11 GL_RENDERER probe) + apitrace (eglretrace, GL trace replay)
# are stock Fedora packages; gfxreconstruct (gfxrecon-replay, VK trace replay) isn't packaged, so
# build it in-guest once and install to /opt/gfxreconstruct (the path
# crates/limina-test/tests/venus_replay.rs hardcodes). This bloats the base — acceptable: it's the
# stock/enhanced *test* base (stock.test / enhanced.test snapshot from it), never a daily-driver
# image, and the enhanced *delivery* (install-enhanced.sh: kernel+mesa+mutter+agent) does NOT ship
# these, so a migrated guest stays clean.
command -v glmark2-es2 >/dev/null && command -v eglretrace >/dev/null \
  || sudo dnf install -y glmark2 glmark2-wayland apitrace
if [ ! -x /opt/gfxreconstruct/bin/gfxrecon-replay ]; then
  sudo dnf install -y gcc-c++ cmake ninja-build git lz4-devel libzstd-devel zlib-devel \
                      xcb-util-keysyms-devel xcb-util-devel vulkan-headers vulkan-loader-devel
  rm -rf "$HOME/gfxreconstruct"
  git clone --depth 1 --recurse-submodules https://github.com/LunarG/gfxreconstruct "$HOME/gfxreconstruct"
  cmake -B "$HOME/gfxreconstruct/build" -G Ninja -S "$HOME/gfxreconstruct" \
    -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/opt/gfxreconstruct \
    -DCMAKE_INSTALL_LIBDIR=lib64 -DGFXRECON_AGENT_BUILD=OFF -DBUILD_WERROR=OFF \
    -DLZ4_OPTIONAL=ON -DZSTD_OPTIONAL=ON
  cmake --build "$HOME/gfxreconstruct/build" -j2   # -j2: -j4 OOMs a 4 GiB guest (perf-ledger.sh)
  sudo cmake --install "$HOME/gfxreconstruct/build"
  # the layer manifest ships a relative library_path; point it at the installed absolute .so
  sudo sed -i 's|"library_path": "[^"]*"|"library_path": "/opt/gfxreconstruct/lib64/libVkLayer_gfxreconstruct.so"|' \
    /opt/gfxreconstruct/share/vulkan/explicit_layer.d/*.json 2>/dev/null || true
fi

echo "-- no-idle-lock gschema override"
sudo tee /usr/share/glib-2.0/schemas/90-limina-no-idle-lock.gschema.override >/dev/null <<'OVR'
[org.gnome.desktop.session]
idle-delay=uint32 0

[org.gnome.desktop.screensaver]
lock-enabled=false
idle-activation-enabled=false
OVR
sudo glib-compile-schemas /usr/share/glib-2.0/schemas/

echo "-- console args (GOP framebuffer + PL011 serial)"
sudo grubby --update-kernel=ALL --args="console=tty0 console=ttyAMA0"

echo "-- SELinux: relabel what we created + clear any stale full-relabel flag"
# Fedora-built images boot enforcing with intact labels (no relabel LOOP like the F43 dev images),
# but the base can still ship /.autorelabel — a request for a one-time first-boot relabel. A
# read-only L2 boot can't consume that flag (root is RO), so it would trip the boot test's guard.
# We restorecon the files we just created (tee/install can mislabel) and clear the stale flag; the
# image already boots enforcing, so its labels are correct.
sudo restorecon -F /etc/sudoers.d/91-limina-nopasswd /etc/gdm/custom.conf \
    /usr/share/glib-2.0/schemas/90-limina-no-idle-lock.gschema.override 2>/dev/null || true
sudo rm -f /.autorelabel

cat <<DONE

==> accessible setup complete for '$USER_NAME'.
    Verify:  loginctl (autologin session present) ; vulkaninfo | head
    Then clean-poweroff and save the disk on the host as the accessible base:
        sudo systemctl poweroff
        # host:  mv <booted-clone>.raw Fedora-Workstation-<REL>.accessible.raw
DONE
