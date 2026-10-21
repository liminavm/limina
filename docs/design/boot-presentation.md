# Boot presentation: one picture from power-on to the desktop and back

Status: design agreed with the synoik side; not built. Owner of the firmware half: Cardea
(`third_party/cardea`); of the guest half: synoik (`docs/fork/boot-splash.md` in the synoik repo).

## Goal

From the moment a VM's window appears until the greeter, and from "Power off"/"Restart" until the
window closes or the next boot's splash, the screen shows one picture: **black, the boot logo at a
fixed place, and whatever the guest's splash adds (spinner, distro watermark)**. No text, no black
frame, no window churn. Text appears only when a human needs it (a menu, a prompt, an error).

Scope: the ACPI machine (the default). The logo reaches the guest through the ACPI BGRT, so the
device-tree machine and `--kernel` boots keep today's behaviour.

## The contract: ACPI BGRT, not a limina protocol

The firmware draws the logo and publishes it, with its position, in the BGRT. Everything after
the firmware places it from there, so nobody needs to know about limina:

- **Stock tier:** Fedora's plymouth `bgrt` theme (two-step `view_set_bgrt_background`) redraws the
  logo at the published offsets and adds its spinner and watermark.
- **Enhanced tier:** synoik-splash, a port of the same placement rule
  (`synoik-splash/src/layout.rs`, `place_firmware`), reads `/sys/firmware/acpi/bgrt`, draws from the
  first DRM frame and hands the card to the synoik system compositor, which keeps the picture until
  power-off.
- Debian's and Arch's default plymouth themes are not `bgrt` (unverified); they still boot, with a
  visible theme change at the splash.

The placement rule both follow: if the published x offset centres the image on the panel, the
offsets are used as-is; otherwise the image is re-centred at x centred, y = 0.382·H − h/2. So the
firmware publishes **x centred and the logo's vertical centre at 38.2% of the height**, the
position every PC with a BGRT uses, and everyone agrees without re-centring.

## Stage by stage

| Stage | Today | Design |
|---|---|---|
| Window opens, worker starting | black | the host pre-paints black + the logo by the firmware's rule, from the same BMP |
| Firmware init | a black flush, a clear, then the logo (centred) | the first presented frame is already black + logo at 38.2% |
| Bootloader (GRUB, sd-boot) | ConOut text on glass; GRUB's clear wipes the logo; `bli` prints an error on every Fedora boot | quiet boot: text to serial only, the logo stays (below) |
| ExitBootServices → `virtio_gpu` | host holds the last firmware frame (Blt-only GOP, no efifb/simpledrm; the device reset sends no `scanoutgone`) | unchanged; any zeroed-buffer flip by the guest is to be measured and fixed guest-side |
| Splash → desktop | plymouth bgrt / synoik-splash → compositor | unchanged |
| Reboot | host holds the guest's last frame across the worker swap; firmware then flashes black | held frame → firmware's black + logo → next splash: one splash with a brief spinner pause |
| Power-off | window and process vanish at once | hold the final frame, fade the window out (~200 ms), exit |

## Firmware (Cardea)

1. **First frame = black + logo.** Draw the logo before the first flush: today `Gpu::set_mode`
   flushes the zeroed buffer and `gfxcon::attach` clears again before `logo::show` draws, which is
   the black flash between the held frame and the logo on every reboot.
2. **Logo at 38.2%,** x centred; the BGRT carries the drawn position.
3. **Per-VM logo.** The embedded logo is the default; a VM may supply its own BMP, which the VMM
   hands to the firmware (a `/chosen` property next to `libkrun,extra-bootargs`, size-capped and
   parsed by the hostile BMP reader). The host's pre-paint uses the same file.
4. **Quiet boot.** While quiet, ConOut text and clears go to serial (every byte: the L2 tests key on
   GRUB's "Booting" line) and the display keeps the logo. The display switches to the text console,
   once and for good, when:
   - a key reaches ConIn before ExitBootServices;
   - the loader has been *reading ConIn* for longer than ~0.5 s, which is a menu countdown, a GRUB
     cryptodisk prompt or an interactive error. A loader that is merely slow (a large initramfs
     over virtio-blk) does not read ConIn and is never revealed;
   - Cardea's own "no bootable device" path runs;
   - the host asked for the boot menu (below).
   On reveal the console is repainted from the text Cardea has kept since the last clear, so a
   prompt already printed is not lost.
5. **Getting to the menu.** The host's "show boot menu on next start" action (part of the host
   boot policy in `firmware-and-acpi.md`) starts the firmware revealed and, for loaders that
   hide their menu, presses the loader's key for it: sets `LoaderConfigTimeoutOneShot` for
   systemd-boot, holds Esc/Shift in ConIn for GRUB's hidden menu (to verify per distro).

## Host (limina)

1. **Pre-paint** the window black + the logo at the firmware's rule and size before the first
   frame, so the firmware's first frame is pixel-identical. No watermark: that is the distro's.
2. **Reboot** keeps today's held frame; nothing new beyond the firmware's first-frame fix.
3. **Power-off** holds the final frame and fades the window out over ~200 ms before exiting
   (`window/mod.rs`, the exited-worker arm that calls `std::process::exit`).
4. **All displays.** When the VM uses all displays, the secondary windows follow the main
   window's lifecycle: kept across reboot and suspend (parked under the same scrim, resumed by the
   same click), closed only at power-off, with the fade. Today they close on every worker exit
   (`window/windows.rs`, `close_secondaries` on `exited`) and on a parked suspend, then reopen when
   the guest re-announces its connectors. The firmware paints head 0 only, so a kept secondary
   shows its held frame (synoik's splash lights every connector) until the guest's splash
   repaints it. A restore must not cycle connectors, so kept windows rebind to the same slots, and
   the suspend splash is saved per display, not only for the main one.
5. **No guest → host signal is needed.** The BGRT and held frames carry the picture; PSCI already
   distinguishes reset from off.

## Seeing the boot

Quiet boot moves text off the glass; it does not throw it away. Two views show it, one per side,
behind one shortcut.

- **Host console (limina).** The worker always keeps the PL011 stream: a bounded ring, plus a log
  per boot in the VM's directory, so a VM that did not boot leaves its evidence behind. A Console
  view (a terminal emulator with input) opens from the menu, and `limina console <vm>` attaches
  from Terminal. It sees what only the host can: firmware, loader, a guest whose display never
  comes up, panics, the end of shutdown, and the kernel and systemd text whenever the guest's
  console is on `ttyAMA0` (synoik images; `serial_console = both`). With input it is also the
  recovery UI: synoik guests put their emergency and rescue shells on the serial console, since
  their card has no fbcon. The window offers "Show console" when the firmware or loader phase
  stalls (Cardea's no-boot path, or no guest frame after N s); a guest wedged after its own first
  frame is not judged by a timer, only by the menu item.
- **Guest details view (synoik).** What the OS prints after the splash starts: the kernel log from
  `/dev/kmsg` (under `quiet` kernel text reaches nowhere else) and `/dev/console` writes, as
  plymouth two-step's details view reads them; after the handover the unprivileged system
  compositor reads the journal instead (Fedora sets `kernel.dmesg_restrict = 1`, so kmsg is
  closed to it). Not built: synoik-splash has no details mode yet.
  Stock images get plymouth's own (Esc).
- **The "boot details" shortcut (host).** Before the guest's first own frame, it opens the host
  Console. After it, the host injects a dedicated details key on its virtual keyboard, which
  synoik-splash and the synoik compositor swallow in every state and turn into the details view.
  The host never injects Esc: it cannot tell a splash from a session, and an Esc closes a user's
  dialog. Stock plymouth images do not know the key, so there the shortcut is the host Console
  only.

## Open

- **The firmware → kernel window.** The guest kernel scans out a zeroed buffer before any splash
  draws: measured 2026-10-21 on a synoik image (Cardea 6ee3439, kernel 7.1.13-limina16k), ~515 ms
  of black on the kernel's own resource ahead of synoik-splash's first frame, almost certainly the
  DRM fbdev client's initial modeset (it happens with fbcon kept off the card;
  `drm_kms_helper.fbdev_emulation=0` removed it on the same image). It is fixed guest-side, not by
  the host dropping black frames (a guest that really draws black would sit on the logo): synoik
  images drop the fbdev client, since synoik-splash probes the connectors itself;
  for the enhanced kernel, a fbdev client that defers its modeset until fbcon binds is the
  upstreamable fix. Stock images keep the black gap (degraded, not broken).

  The same reboot measured the firmware gaps this design closes: two all-black Cardea frames
  (~70 ms) before the logo, the logo on glass for ~17 ms, then GRUB's clear and its "Booting …"
  line for ~1.2 s. The shutdown side was clean: the compositor's last frame stayed until the
  firmware's first flush. A power-off on the same image ended on the splash, still for ~216 ms
  until the worker exited, and then the window vanished.
- **Which key is the details key** (`KEY_INFO`, or a limina vendor key), and whether the synoik
  compositor opens the same log over a greeter or session or ignores the key there.
- **Retina.** The firmware runs at the window's physical pixels and draws the logo 1:1, and the
  splashes draw at scale 1, so they match but the logo is small on a Retina host. Agreed form:
  the BGRT image is what is on glass. The firmware draws a 2x logo at backing scale 2 and
  publishes that image and its offsets, and a splash treats the BGRT image as device pixels,
  never scaling it by the output scale (plymouth two-step does; synoik will not).
- **A Cardea-native loader** (BLS Type #1 entries, UKIs, `kernel-install`/grubby compatible) is a
  separate later project; with quiet boot it buys boot time and robustness, not looks.
