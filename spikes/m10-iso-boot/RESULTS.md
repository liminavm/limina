# M10 Phase 3a — boot an EFI-bootable aarch64 ISO

**Question (design §11, the "only real unknown"):** when an EFI-bootable aarch64 ISO is the
*sole* disk, does limina's firmware BDS detect its El Torito EFI image and launch the ISO's
bootloader — with no `--kernel`, no separate root disk, no code changes beyond M10's `--cdrom`?

**Answer: YES — out of the box.** No new code was needed. `--cdrom ISO` already forwards a
read-only `--disk` (so the ISO is `vda`), and our GOP EDK2 firmware already carries the full
optical-boot driver stack (`PartitionDxe` with El Torito, `FatPkg/EnhancedFatDxe`, `VirtioBlkDxe`).
The firmware self-discovers the bootable device and chainloads GRUB.

## Vehicle

```
spikes/m10-iso-boot/boot-iso.sh      # ISO as sole disk, debug-GOP firmware, serial + sw-2D capture
ISO: Fedora-Server-netinst-aarch64-43-1.6.iso   (gitignored; 1.1 GB; dl.fedoraproject.org)
FW:  target/krun-efi/KRUN_EFI.gop.debug.fd       (verbose EDK2 BDS → PL011 serial)
```

`target/debug/limina --firmware <GOP> --cdrom <ISO> --console serial.log --gpu-software-2d --display-capture grub.png --display-size 1024x768`

## Evidence — two independent channels agree

**(1) Serial (verbose EDK2 BDS):** the full boot chain is in `serial.log`:

```
VirtioBlkInit: LbaSize=0x200[B] NumBlocks=0x23B8F4[Lba]          # virtio-blk backed by the ISO (~1.1 GB)
PartitionDxe: El Torito standard found on handle 0x13F56A898.    # El Torito boot catalog detected
Installed Fat filesystem on 13F4AA598                            # embedded ESP (FAT) mounted
FSOpen: Open '\EFI\BOOT\BOOTAA64.EFI' Success                    # found the EFI bootloader
BdsDxe: starting Boot0001 "UEFI Misc Device"
        from VenHw(...)/CDROM(0x0,0x9C,0x112E0)/\EFI\BOOT\BOOTAA64.EFI   # launched from the CDROM device path
FSOpen: Open '\EFI\BOOT\grubaa64.efi' Success                    # shim chainloaded GRUB
GRUB version 2.12   ...   Press enter to boot the selected OS    # GRUB MENU REACHED
```

The GRUB menu rendered in full over the firmware console (serial + GOP): entries
`Install Fedora 43`, `*Test this media & install Fedora 43` (default), `Troubleshooting -->`,
with a live `60s → …` countdown.

**(2) GOP scanout (`grub.png`, 1024×768):** the same GRUB 2.12 menu, rendered by the ISO's
bootloader over the firmware's GOP → virtio-gpu, captured via software-2D. Visual confirmation
that matches the serial byte-for-byte. (Saved alongside — regenerate with `boot-iso.sh`.)

**Bonus (exceeds the bar):** left to auto-boot, the default entry loaded the **installer kernel +
initrd from the ISO** and ran:

```
[drm] Initialized virtio_gpu ...        # installer kernel's own drivers came up
ISO 9660 Extensions: Microsoft Joliet Level 3
loop0: detected capacity change from 0 to 1816328   # the install image loop-mounted
/dev/vda:  6b84bc4c70ac389457dcfcf81950fdeb          # Anaconda media check reading our ISO (vda)
Press [Esc] to abort check.
```

So the *entire* El Torito path works end-to-end: firmware → GRUB → installer kernel+initrd →
ISO 9660 mount → Anaconda, all off the single `--cdrom` device.

## Conclusion

Phase 3a needs **zero code**. The `--cdrom` sugar (M10 Phase 2) + our GOP firmware already boot an
unmodified Fedora aarch64 installer ISO to its bootloader and into the installer. A regression test
(`crates/limina-test/tests/disks.rs::boots_efi_iso_to_bootloader`) locks this in by booting the ISO
as the sole disk and asserting the firmware reaches GRUB on the console (gated on the ISO's
presence — it is gitignored & large, so the test SKIPs when absent, like the other image-gated L2s).

Phase 3b (a persistent EFI varstore + host-managed BootOrder, so an installed-to-a-target-disk guest
survives the ISO being detached) remains **deferred** — it is a productization concern, not a boot
blocker, and is filed in the design doc / Milestone 11.
