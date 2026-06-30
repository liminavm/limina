#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# M10 Phase 3a spike: boot an EFI-bootable aarch64 ISO as the SOLE disk and prove the
# firmware BDS detects its El Torito EFI image and launches the ISO's bootloader (GRUB).
#
# Evidence (two independent signals):
#   - serial.log  : verbose EDK2 BDS debug (the GOP *debug* firmware logs to PL011) —
#                   shows the boot device discovery + which EFI app BDS launches.
#   - grub.png    : the GOP scanout captured via software-2D — the GRUB menu rendered
#                   by the ISO's bootloader over the firmware's GOP (visual confirmation).
#
# The ISO is read-only; --cdrom forwards it as the only --disk → it is vda. No --kernel,
# no --disk: the firmware must self-discover the bootable device. Runs headless in the BG;
# kill after the menu has had time to render.
set -euo pipefail
cd "$(dirname "$0")/../.."

ISO=${ISO:-Fedora-Server-netinst-aarch64-43-1.6.iso}
FW=${FW:-target/krun-efi/KRUN_EFI.gop.debug.fd}
OUT=spikes/m10-iso-boot
SERIAL=$OUT/serial.log
PNG=$OUT/grub.png

[ -f "$ISO" ] || { echo "ISO not found: $ISO"; exit 1; }
[ -f "$FW" ]  || { echo "firmware not found: $FW (build with GOP=1 scripts/build-krun-efi.sh)"; exit 1; }

rm -f "$SERIAL" "$PNG"
echo "==> booting $ISO as sole disk on $FW (headless, software-2D capture)"
exec target/debug/limina \
  --firmware "$FW" \
  --cdrom "$ISO" \
  --console "$SERIAL" \
  --gpu-software-2d \
  --display-capture "$PNG" \
  --display-size 1024x768
