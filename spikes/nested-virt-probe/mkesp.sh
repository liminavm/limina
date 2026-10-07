#!/bin/sh
# Build a whole-disk FAT32 ESP that boots the probe the way a real guest boots: firmware ->
# GRUB (\EFI\BOOT\BOOTAA64.EFI) -> kernel + initramfs. Take grubaa64.efi from any Fedora
# image's ESP (EFI/fedora/grubaa64.efi). macOS only (hdiutil, newfs_msdos), no root needed.
#
# usage: mkesp.sh <kernel Image> <initramfs.cpio> <grubaa64.efi> <out.img>
set -e
kernel=$1 initrd=$2 grub=$3 out=$4
mnt=$(mktemp -d)
dd if=/dev/zero of="$out" bs=1m count=96 2>/dev/null
dev=$(hdiutil attach -imagekey diskimage-class=CRawDiskImage -nomount "$out" | awk '{print $1; exit}')
newfs_msdos -F 32 -v ESP "$dev" > /dev/null
mount -t msdos "$dev" "$mnt"
mkdir -p "$mnt/EFI/BOOT" "$mnt/EFI/fedora"
cp "$grub" "$mnt/EFI/BOOT/BOOTAA64.EFI"
cp "$kernel" "$mnt/EFI/BOOT/vmlinuz"
cp "$initrd" "$mnt/initrd.cpio"
cfg='set timeout=0
menuentry "nested probe" {
  linux /EFI/BOOT/vmlinuz console=ttyAMA0 rdinit=/init
  initrd /initrd.cpio
}'
# Fedora's GRUB looks for its config next to itself or under EFI/fedora; write both.
printf '%s\n' "$cfg" > "$mnt/EFI/BOOT/grub.cfg"
printf '%s\n' "$cfg" > "$mnt/EFI/fedora/grub.cfg"
umount "$mnt"
hdiutil detach "$dev" > /dev/null
rmdir "$mnt"
echo "built $out"
