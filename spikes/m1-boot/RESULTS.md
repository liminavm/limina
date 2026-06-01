# Spike: M1 boot — Fedora-Workstation-43.raw via EFI on libkrun

**Question (roadmap M1 risks):** Does the local Fedora `.raw` boot end-to-end on
libkrun via EFI? Where does the EFI firmware come from? What is the disk/rootfs
layout? Can we get a console?

**Verdict: YES — boots cleanly to systemd userspace.** All M1 boot risks resolved.
This was done as a *link spike against the Homebrew libkrun bottle (1.17.4)* — no
from-source build needed to answer the boot question. Host: macOS 26.5, M1 Max.

## How to reproduce

`./run.sh` (un-sandboxed: needs codesign). It builds `boot.c` against
`/opt/homebrew/lib/libkrun.dylib`, ad-hoc-signs it with
`com.apple.security.hypervisor`, and boots the image. Watch `console.log`.

## What boots, and how

- **EFI firmware:** there is **no** `libkrunfw-efi.dylib` in Homebrew (the roadmap
  said there was — corrected). `krun_set_firmware(ctx, path)` simply `std::fs::read`s
  a flat EDK2 blob into guest RAM (`builder.rs:1568`, `Payload::Firmware`). We use
  **krunkit's** blob: `/opt/homebrew/Cellar/krunkit/1.2.1/share/krunkit/KRUN_EFI.silent.fd`
  (2 MiB, `edk2-13e8adac8a`, ArmVirt-derived). The bottle exports every symbol M1
  needs (`krun_set_firmware`, `krun_add_disk2`, `krun_disable_implicit_console`,
  `krun_add_serial_console_default`, …).
- **Launch sequence:** `krun_create_ctx` → `krun_set_vm_config(vcpus=4, ram=4096)`
  → `krun_set_firmware(KRUN_EFI.silent.fd)` → `krun_add_disk2("root", raw, RAW, ro)`
  → console (see below) → `krun_start_enter` (never returns; runs in a child process
  exactly as the architecture requires).
- **Boot chain (all captured on serial):** EDK2 → BdsDxe → **shim → GRUB 2.12** →
  reads the BLS entries → boots `Fedora Linux 6.19.13-200.fc43.aarch64` → kernel →
  **systemd PID 1** → mounts root + /boot **read-write** → reaches
  `getty.target (Login Prompts)`. Full dmesg in `boot-console.clean.log`.

## Disk / rootfs layout (resolved)

It's an **MBR** disk (not GPT; `hdiutil` can't read it — parse the table directly),
a Fedora **aarch64** generic/Raspberry-Pi image. libkrun exposes it as `vda`:

| Part | MBR type | Size | FS | Mount | Notes |
|------|----------|------|----|-------|-------|
| vda1 | 0x06 | 0.52 GB | FAT | ESP (`/boot/efi`) | start LBA 34816 = **17 MiB** offset; **mountable on macOS** |
| vda2 | 0xea | 2.10 GB | ext4 | `/boot` | XBOOTLDR; fs-uuid `572bbaa7-43f9-4a0f-bfa7-28854a3e2b78`; holds GRUB + BLS + kernels |
| vda3 | 0x83 | 61.8 GB | btrfs | `/` | fsid `bad1888f-d2d7-43cd-b70c-4e6f36df4c66`, `subvol=root`, zstd:1 |

The kernel+initramfs in `/boot` carry every driver, so the stock distro boots with
**zero** limina guest components — this is the M1 compatibility floor, confirmed.

## Console findings (the fiddly part)

libkrun has two console-visibility traps on the EFI path:

1. **The implicit firmware serial is output-dropped.** On firmware boot libkrun
   creates a legacy PL011 with `None, None` (`builder.rs:731`; the `io::stdout()`
   line is commented out, labeled "Uncomment this to get EFI output"). And the
   firmware blob is the **silent** EDK2 build. So a naive boot is completely blind —
   `krun_set_console_output` does **not** redirect it (it targets the bundled-kernel
   implicit console, not the firmware serial). Fix without patching libkrun:
   `krun_disable_implicit_console()` + `krun_add_serial_console_default(in, out)` —
   our serial becomes the first/only one (`ttyS0`/PL011) that EDK2 uses as ConOut,
   with output wired to our fd. → **EDK2 + GRUB now fully visible.**
   - Quirks: the 1.17.4 bottle asserts `input_fd != -1` and registers it with
     kqueue, so a backgrounded `stdin` panics (`epoll.rs:181`). Give it a **pipe or
     FIFO read-end** (pollable, no EOF). Serial **input** never reaches GRUB —
     the silent firmware doesn't wire ConIn to the PL011 — so interactive GRUB over
     serial is not available out of the box.

2. **The guest kernel won't talk on serial without an explicit `console=`.** Stock
   Fedora's BLS cmdline has none, and the kernel doesn't auto-find libkrun's PL011
   (KRUN_EFI's ACPI doesn't advertise an SPCR for it). GRUB shows, then silence.

### Making the kernel talk on serial

libkrun's PL011 is the **first** MMIO device, at **`0x0a001000`** (`MMIO_MEM_START`
`0x0a000000` + one `MMIO_LEN` `0x1000`; serial is registered before rtc/gic/gpio —
`builder.rs:1894`, `device_manager/hvf/mmio.rs:156`). Booting with
`earlycon=pl011,mmio32,0x0a001000 keep_bootcon console=ttyAMA0,115200 console=tty0`
gives the **full kernel dmesg** over serial (filesystem mounts, zram swap,
`Free page reporting enabled`, `getty.target`). It still goes quiet ~4.4 s in once
`tty0` becomes the primary console and the PL011 bootcon is dropped; and there is no
interactive `login:` on serial — Fedora **Workstation** boots to graphical `gdm`,
and no `serial-getty@ttyAMA0` attaches because the real `amba-pl011` driver has no
ACPI/DT node to bind. **For limina this is a non-issue: the real console is the M2
display.** Serial is a debug aid.

`serial-grub.cfg` is the reusable recipe. To apply it to the image's ESP from macOS
(FAT, so mountable — ext4 `/boot` is not): extract the ESP partition
(`dd ... bs=1m skip=17 count=500`), `hdiutil attach` it, copy `serial-grub.cfg` over
`/EFI/BOOT/grub.cfg` and `/EFI/fedora/grub.cfg`, detach, `dd` it back at `seek=17`.
**The image was returned to pristine stock after this spike** (original stubs
restored) to keep the M1 compatibility floor honest — re-apply only when debugging.

## Implications for the build

- **M1 boot path is validated end-to-end.** The from-source libkrun build is still
  required for the product (patches, 1.18 APIs, GPU/input) but is **not** needed to
  boot — the bottle is enough for the CLI skeleton.
- **One clean upstreamable libkrun patch is worth carrying:** wire the firmware-boot
  serial's output to a configurable fd (turn the commented `io::stdout()` into the
  `krun_set_console_output` fd) so the EFI debug console doesn't need the
  disable-implicit + add-serial dance. Mechanism in libkrun, policy in limina.
- **Observed for later milestones:** `Free page reporting enabled` (virtio-balloon
  free-page-reporting is live in the guest — relevant to M6 dynamic memory); disk is
  `vda` virtio-blk; guest is 4 KiB-paged as expected.
