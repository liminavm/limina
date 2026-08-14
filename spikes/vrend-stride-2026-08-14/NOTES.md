# vrend/KK stride corruption: KosmicKrisp silently substitutes an unaligned row pitch

Found on the dogfood guest 2026-08-14, within minutes of rebooting it onto the **supported**
enhanced configuration (`GALLIUM_DRIVER=virgl` + `MESA_LOADER_DRIVER_OVERRIDE=virtio_gpu`, GL on
vrend). Guest was reverted to the retired zink config afterwards, so this is captured evidence,
not a live system. **Not yet reproduced locally — that is the next step.**

## Symptom

Windows render with **diagonal shear**: content smeared into sloping streaks, as though each row
were read at a slightly different offset than it was written (`corruption-2026-08-14.png`). The
user reports **resizing the window "just right" makes it self-heal**.

**The split is by rendering path, not by application or by width.** In the capture the sheared
window is **Firefox (GL -> vrend)**; the terminal behind it is **ghost, a Vulkan client
(-> venus)**, and none of ghost's windows are affected. So a Vulkan client at the same width
renders correctly.

That is the key discriminator: GL surfaces travel the vrend path and hit the explicit-pitch
import; Vulkan surfaces go through venus and never do. It also explains the timing exactly —
the bug surfaced the moment GL was flipped to vrend on 2026-08-04.

## Mechanism

The host log (`supervisor-excerpt.log`) carries, from our own KK modifier path:

```
[KK-MODIFIER] EXPLICIT rowPitch 7900 unusable (minimum 7904, alignment 16); using the computed pitch
[KK-MODIFIER] EXPLICIT rowPitch 8600 unusable (minimum 8608, alignment 16); using the computed pitch
[KK-MODIFIER] EXPLICIT rowPitch 5780 unusable (minimum 5792, alignment 16); using the computed pitch
[KK-MODIFIER] EXPLICIT rowPitch 8300 unusable (minimum 8304, alignment 16); using the computed pitch
[KK-MODIFIER] EXPLICIT rowPitch 1112 unusable (minimum 1120, alignment 16); using the computed pitch
[KK-MODIFIER] EXPLICIT rowPitch    4 unusable (minimum   16, alignment 16); using the computed pitch
```

The guest hands down an **explicit** row pitch; KosmicKrisp requires 16-byte alignment; the pitch
is not aligned; KK **silently substitutes its own computed pitch and proceeds**. The guest then
addresses the image at its own stride while the texture is laid out at KK's — every row drifts by
the difference, which is the shear.

Frequency in one session: 28 `KK-MODIFIER` lines, led by `7900` (12 occurrences).

## Three independent confirmations

1. **The log** shows the substitution happening, with the alignment arithmetic explicit.
2. **The width matches.** The corrupted window measures ~1,970 px wide in the capture; the
   most-frequent rejected pitch, 7900 B, is exactly 1975 px x 4 B. The window whose pitch KK
   refused is the window that is broken.
3. **Resize heals it.** At a width whose pitch happens to land on 16-byte alignment there is no
   substitution and no shear — precisely what the theory predicts.
4. **A Vulkan client alongside it is clean.** ghost (venus) shares the same display, compositor
   and host at the same moment and shows no shear, which rules out the compositor, the scanout
   and the display path, and localises the fault to the GL/vrend import.

### One thing that does NOT fit yet

A constant 4-byte (1-pixel) drift per row predicts streaks at roughly 45 degrees. The captured
streaks are far shallower — on the order of ~7 px of horizontal drift per row. So the simple
"guest writes at 7900, texture is laid out at 7904, every row slips one pixel" story explains
that corruption *happens* but not its exact geometry.

Possible explanations, none verified: the mismatch applies per tile rather than per row; the
compositor is repainting damage rectangles rather than the full surface; or the effective pitch
delta is larger than the logged 4 bytes. **Do not treat the mechanism as fully understood until
the observed slope is derived from the numbers** — an unexplained factor of ~7 is exactly the
kind of gap that later turns out to be the real bug.

## Redaction

`corruption-2026-08-14.png` is cropped from the original 3840x2160 capture. The full screenshot
included a window title bar carrying the dogfood Mac's hostname, which the pre-commit token scan
cannot see inside a PNG. **The un-redacted original was never committed and no longer exists in
the working tree.** Any future capture from that machine needs the same manual check — the hook
protects text, not pixels.

## Why it appeared now

This is the GL path the 2026-08-04 flip made default. On the retired zink-as-guest-GL config the
surfaces travelled a different allocation path, so the unaligned-pitch import was not exercised
the same way. It is **our own code** — the KK modifier extension (LINEAR-only) plus vkr
passthrough — not an upstream bug.

## The real defect is the silent substitution

Whatever the right pitch policy turns out to be, **succeeding with different parameters than the
caller asked for is the bug**. A rejected import that falls back is recoverable; a silent
substitution guarantees corruption and is invisible to the guest. Same trust-boundary shape as
the earlier empty-clear-rect fix, which was resolved at the vkr boundary.

Candidate fixes, in rough order of preference:

1. **Advertise the alignment requirement** so the guest allocates a conforming pitch in the first
   place (the modifier/format properties are the channel).
2. **Fail the import** when the explicit pitch is unusable, letting the caller take its existing
   fallback path, rather than substituting.
3. Round the allocation up host-side *and* report the corrected pitch back — only if the caller
   actually honours a returned pitch, which needs checking before relying on it.

## Reproducing locally (next step)

Nothing here has been reproduced off the dogfood machine yet. Plan:

- Boot a local enhanced image with the supported env (`GALLIUM_DRIVER=virgl`,
  `MESA_LOADER_DRIVER_OVERRIDE=virtio_gpu`) via `cargo xtask run --disk <enhanced.raw>`.
- Open a **GL** client (Firefox, or anything on vrend) and size it to a width whose pitch is not
  16-byte aligned — width x 4 B mod 16 != 0, e.g. 1975 px (pitch 7900). Widths divisible by 4
  give aligned pitches and will NOT reproduce.
- Keep a **Vulkan** client open beside it as the built-in control: it should stay clean at the
  same width, as ghost did in the field capture. If it also shears, the fault is not where this
  write-up places it.
- Watch the worker log for `[KK-MODIFIER] ... unusable`; pixel-verify via the IOSurface scanout
  (`spikes/venus-draw-probe/iosdump.swift`, `LIMINA_GLOBAL_SCANOUT=1`) rather than by eye.
- The falsifiable prediction: shear appears exactly at widths where `width * 4 % 16 != 0` and
  vanishes at widths where it is 0.

## Artifacts

- `corruption-2026-08-14.png` — the captured shear (3840x2160 full-screen shot).
- `supervisor-excerpt.log` — host log excerpt covering the episode.
- Guest at capture time: mesa `26.1.5-9.limina.fc44`, kernel `7.1.6-limina16k`, page size 16384.
