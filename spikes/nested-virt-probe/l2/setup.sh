#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Runs as root INSIDE a nested limina guest (Fedora with KVM up). Builds what l2/boot.sh boots
# under QEMU+KVM: a busybox initramfs whose init reports over the serial port, reads a virtio-blk
# disk, and either powers off or keeps ticking. Output in /root/l2.
set -euo pipefail
rpm -q busybox >/dev/null || dnf -y -q install busybox
rpm -q qemu-system-aarch64-core >/dev/null || dnf -y -q install qemu-system-aarch64-core

L2=/root/l2
rm -rf "$L2" && mkdir -p "$L2/root/bin" "$L2/root/proc" "$L2/root/sys" "$L2/root/dev"
cp "$(command -v busybox)" "$L2/root/bin/busybox"
cat >"$L2/root/init" <<'INIT'
#!/bin/busybox sh
/bin/busybox --install -s /bin
mount -t proc proc /proc
mount -t sysfs sys /sys
mount -t devtmpfs dev /dev
echo "L2-UP kernel=$(uname -r) cpus=$(nproc) mem=$(awk '/MemTotal/ {print $2}' /proc/meminfo)kB"
echo "L2-DISK $(md5sum /dev/vda | cut -d' ' -f1)"
if grep -q l2.tick /proc/cmdline; then
    i=0
    while true; do
        echo "L2-TICK $i $(cut -d' ' -f1 /proc/uptime)"
        i=$((i + 1))
        sleep 1
    done
fi
echo L2-DONE
poweroff -f
INIT
chmod +x "$L2/root/init"
(cd "$L2/root" && find . | cpio -o -H newc --quiet | gzip -1) >"$L2/initramfs.cpio.gz"

head -c $((8 << 20)) /dev/urandom >"$L2/disk.img"
md5sum "$L2/disk.img" | cut -d' ' -f1 >"$L2/disk.md5"
echo "l2 kit ready: $(du -sh "$L2" | cut -f1), disk md5 $(cat "$L2/disk.md5")"
