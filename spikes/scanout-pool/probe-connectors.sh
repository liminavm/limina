#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# What does the guest see with a scanout pool of N? Every DRM connector's status, real EDID
# size and mode count, the compositor's monitor list, and the counts of the kernel-side
# virtio-gpu errors — the last so a pool run can be diffed against a pool=1 baseline instead
# of having its dmesg noise attributed to the pool by eye.
#
#   spikes/scanout-pool/probe-connectors.sh <ssh-port>
set -u
PORT="${1:?usage: probe-connectors.sh <ssh-port>}"
ssh_g() {
  ssh -p "$PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
      -o LogLevel=ERROR claude@127.0.0.1 "$@"
}

echo "=== virtio-gpu probe (dmesg) ==="
ssh_g 'sudo dmesg | grep -iE "virtio_gpu|virtio-gpu" | grep -v "response 0x" | head -10'

echo
echo "=== virtio-gpu ctrl errors (diff these against the pool=1 baseline) ==="
ssh_g 'sudo dmesg | grep -oE "response 0x[0-9a-f]+ \(command 0x[0-9a-f]+\)" | sort | uniq -c'

echo
echo "=== DRM connectors ==="
ssh_g 'for c in /sys/class/drm/card*-*/; do
        n=$(basename "$c")
        s=$(cat "$c/status" 2>/dev/null)
        e=$(wc -c < "$c/edid" 2>/dev/null || echo 0)
        m=$(grep -c . "$c/modes" 2>/dev/null || echo 0)
        echo "$n status=$s edid_bytes=$e modes=$m"
      done | sort'

echo
echo "=== framebuffers / memory ==="
ssh_g 'ls /dev/fb* 2>/dev/null | tr "\n" " "; echo; free -m | head -2'

echo
echo "=== compositor monitors ==="
ssh_g 'gdbus call --session -d org.gnome.Mutter.DisplayConfig \
        -o /org/gnome/Mutter/DisplayConfig \
        -m org.gnome.Mutter.DisplayConfig.GetCurrentState 2>/dev/null \
      | tr "," "\n" | grep -E "Virtual-|display-name" | head -12 \
      || echo "(no mutter DisplayConfig on the session bus)"'

echo
echo "=== monitors.xml ==="
ssh_g 'grep -c "<configuration>" ~/.config/monitors.xml 2>/dev/null \
       | sed "s/^/configuration stanzas: /" || echo "no monitors.xml"'
