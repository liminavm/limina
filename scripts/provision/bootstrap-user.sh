#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Create the test images' user in a pristine Fedora Workstation image, standing in for
# gnome-initial-setup, so make-accessible.sh can run over ssh with nobody at the window.
#
# A fresh Workstation image has no user and no running sshd. systemd reads both halves of the way
# in from SMBIOS OEM strings, so boot a clone of the pristine image with
#
#   ssh.authorized_keys.root               the host's public keys (root's authorized_keys)
#   systemd.unit-dropin.multi-user.target  "[Unit]\nWants=sshd.service"
#
# each base64-encoded as `--smbios-oem-string io.systemd.credential.binary:<name>=<base64>`
# (the binary form, because neither value survives the boot vehicle's word splitting as text),
# then run this as root:
#
#   ssh -p <PORT> root@127.0.0.1 'bash -s' < scripts/provision/bootstrap-user.sh
#
# It creates `claude` (password `claudiusrobotus`, wheel, NOPASSWD sudo) with root's keys, marks
# gnome-initial-setup done for it and enables sshd for every later boot. Reboot afterwards: GDM
# leaves its initial-setup greeter only when it next starts and finds a user. Idempotent.
set -euo pipefail
U="${LIMINA_GUEST_USER:-claude}"
id "$U" >/dev/null 2>&1 || useradd -m -G wheel -c Claude "$U"
echo "$U:claudiusrobotus" | chpasswd
install -d -m 0700 -o "$U" -g "$U" "/home/$U/.ssh"
install -m 0600 -o "$U" -g "$U" /root/.ssh/authorized_keys "/home/$U/.ssh/authorized_keys"
install -d -m 0755 -o "$U" -g "$U" "/home/$U/.config"
echo yes > "/home/$U/.config/gnome-initial-setup-done"
chown "$U:$U" "/home/$U/.config/gnome-initial-setup-done"
echo "$U ALL=(ALL) NOPASSWD: ALL" > /etc/sudoers.d/91-limina-nopasswd
chmod 0440 /etc/sudoers.d/91-limina-nopasswd
systemctl enable sshd
restorecon -RF "/home/$U" /etc/sudoers.d/91-limina-nopasswd
echo "bootstrap done: $(id "$U")"
