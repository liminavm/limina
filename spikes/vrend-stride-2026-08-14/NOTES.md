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

### Which side is which (corrected 2026-08-14 after the local repro)

The field write-up above localised the fault to "the GL/vrend import". That is the wrong half.
**vrend is the exporter, venus is the importer**, and it takes both:

- The **GL client** renders through virgl/vrend, which allocates its buffer at *vrend's* stride —
  tight-packed `width * 4`, with no knowledge of Metal's 16-byte linear-texture row alignment.
- The **compositor** imports that dmabuf through **Vulkan/venus**, passing the client's stride
  verbatim as `VkImageDrmFormatModifierExplicitCreateInfoEXT.pPlaneLayouts[0].rowPitch`
  (`testcomp/src/vk.rs:467`, transcribed from synoik's own importer).
- KK checks `explicit >= computed && explicit % align == 0` (`kk_image_layout.c:264`), fails the
  alignment test, and substitutes.

This is why the compositor matters. **mutter composites with GL**, so a GL client's buffer never
crosses into a Vulkan import and the path is never reached: a full mutter session at a known-bad
width produced **zero** substitutions. That is a false negative, not a refutation — an important
distinction, because a mutter-based "cannot reproduce" would have looked like evidence against
the whole theory.

It also names the false premise in KK's own comment at the emission site: *"in the VM stack the
exporter's image at the same width computes the same pitch, so this stays coherent end to end."*
That holds when both ends are KK images. It does not hold when the exporter is vrend, which
computes its stride by a different rule entirely.

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

## REPRODUCED LOCALLY, 2026-08-14

On `Fedora-Workstation-44.enhanced.synoik.raw` (see `docs/images.md`): synoik as the session
compositor, supported enhanced env verified at `/proc/<pid>/environ`, a GL client
(`glmark2-wayland`, `GL_RENDERER: virgl (zink … MESA_KOSMICKRISP)`) driven across exact buffer
widths by `width-sweep.sh` in this directory.

| width | pitch | pitch % 16 | predicted | observed |
|---|---|---|---|---|
| 1972 | 7888 | 0 | clean | clean (0 substitutions) |
| 1974 | 7896 | 8 | shear | **6 substitutions** |
| 1975 | 7900 | 12 | shear | **6 substitutions** |
| 1976 | 7904 | 0 | clean | clean (0) |
| 1978 | 7912 | 8 | shear | **6 substitutions** |
| 1980 | 7920 | 0 | clean | clean (0) |

**6/6 against the log-line prediction**, alternating with width. The 1975 line is byte-identical
to the field capture: `EXPLICIT rowPitch 7900 unusable (minimum 7904, alignment 16)`.

The dogfood machine is no longer needed for this bug.

## THE LOG LINE IS NOT THE BUG (pixel measurement, same day)

The table above uses `[KK-MODIFIER]` as the oracle. **It is the wrong oracle**, and following it
would have produced a fix that changed nothing. Pixels say something different and worse.

Captured the composited output (guest-side `grim` under synoik — it re-samples the imported
texture, so the shear shows) and measured the per-row drift with `measure-shear.py`:

| width | `w*4` | KK's pitch (align 16) | **real IOSurface bpr** | Δ | predicted drift | **measured** |
|---|---|---|---|---|---|---|
| 1968 | 7872 | 7872 | 7936 | 64 | 16 px | **16.000** |
| 1974 | 7896 | 7904 | 7936 | 32 | 8 px | **7.999** |
| 1976 | 7904 | 7904 | 7936 | 32 | 8 px | **8.000** |
| 1980 | 7920 | 7920 | 7936 | 16 | 4 px | **4.014** |

Three things follow, and the second one is the finding:

1. **The real row stride is 256-byte aligned**, not 16: `bpr = align_up(w*4, 256) = 7936` for
   every width above. Read straight off the surface (`iosdump` now prints `bpr`), not inferred —
   a 1968-wide surface reports `bpr=7936 (w*4=7872, pad=64)`.
2. **Width 1976 emits NO warning and is corrupt anyway.** Its guest pitch (7904) equals KK's
   computed pitch, so the alignment check passes and nothing is logged — but reality is 7936, so
   it shears by the same 8 px/row as 1974. Every one of the "clean (0)" rows in the sweep table
   is a **false negative**. In practice *every* GL client surface is sheared; only `width % 64 == 0`
   would be clean, and that is 1 width in 64 — which is exactly why resizing "just right" healed it.
3. **The drift is fully explained.** `(align_up(w*4,256) - align_up(w*4,16)) / 4` predicts all four
   measurements to within 0.02 px. The field capture's "~7 px/row" was right after all; the
   arithmetic that called for 1–3 px was wrong, because it compared the guest's pitch to KK's
   instead of comparing KK's to the actual allocation.

So the mismatch that corrupts is **KK vs. reality**, not **guest vs. KK**. The `KK-MODIFIER` line
reports a real inconsistency, but a *different, narrower* one that merely correlates with widths.
Had we shipped candidate fix 1 or 2 (advertise the alignment / fail the import), the guest would
have started sending 16-aligned pitches, the warning would have gone silent — and every window
would still have sheared, now with no diagnostic at all.

**Revised fix direction:** on import of external memory, KK must take the row stride from the
*allocation* (the IOSurface's `bytesPerRow`), not recompute it from width and a format alignment.
The computed pitch is only correct for memory KK allocated itself. The exporter reporting its true
stride to the guest is a complementary fix, and the 256-vs-16 gap should be settled at the source
rather than by teaching both sides the same magic number.

**This is the "unexplained factor of ~7" the field write-up flagged as "exactly the kind of gap
that later turns out to be the real bug".** It was. The discipline that paid off was refusing to
accept a log line as proof of pixels.

## FIXED 2026-08-14 (virglrenderer `limina` 5c76245)

The approved direction — "read the stride from the allocation" — turned out to be **not
implementable at KosmicKrisp**, and the reason is worth recording: the client buffer reaches KK as
`VkImportMemoryHostPointerInfoEXT` / `HOST_ALLOCATION_BIT_EXT` (`vkr_device_memory.c`, the
`else if (ptr)` branch) — a raw pointer and a length. No IOSurface handle crosses, and there is no
way to recover one from an address, so KK has nothing to read the stride *from*. (The MTLTEXTURE
import right above it, which *would* carry the layout, only covers the compositor's own scanout
image. It was env-gated off when this was written; the default flipped ON on 2026-08-14, which
does **not** change this conclusion — client buffers still arrive as a bare host pointer, so the
stride fix below remains the load-bearing one. See spikes/modifier-necessity/RESULTS.md.)

So the fix went in on **the opposite side**: `vkr_mtl_iosurface_alloc_plain` now **forces** the
IOSurface's `bytesPerRow` to the pitch a Metal linear texture will use —
`align(width * bpe, minimumLinearTextureAlignmentForPixelFormat:)`, queried from Metal rather than
hardcoded, so it cannot drift out of step with KK's own computation. Same end state (KK's pitch ==
reality), reached from the exporter.

**This is also the better fix, not merely the possible one.** It needs no guest change: a stock
guest still fabricates a tight-packed explicit pitch, KK still rejects it and substitutes its
computed one — and that substitution is now *the truth*. Correct pixels on both tiers for free, and
the guest-goes-truthful arc stays an upstream nicety rather than a prerequisite.

Verified on `Fedora-Workstation-44.enhanced.synoik.raw` (a CoW clone), glmark2-wayland under synoik,
`grim` capture, `measure-shear.py`:

| width | pre-fix drift | **post-fix** | `KK-MODIFIER` lines |
|---|---|---|---|
| 1968 | 16.000 px/row | **-0.002** | 0 |
| 1974 | 7.999 | **-0.051** | **6** |
| 1976 | 8.000 | **-0.002** | 0 |
| 1980 | 4.014 | **-0.002** | 0 |

`iosdump` on a live client surface reads `bpr=7872 (w*4=7872, pad=0)` at width 1968 — align16, where
it was 7936 before. Zero `[KK-STRIDE]` override warnings across 16 production allocations. The user
confirmed by hand that **resizing a Firefox window is now clean at arbitrary widths**, which is the
original field symptom ("resizing just right heals it") inverted.

**Read the 1974 row carefully — it is the whole point.** The warning still fires six times *and the
pixels are clean*. Anyone who takes `[KK-MODIFIER]` as the corruption oracle will read that as a
live bug and "fix" it back. The line reports a real guest-vs-KK disagreement; it never reported the
corruption.

### Does IOSurface honor a forced, non-256 pitch?

The whole approach dies if it doesn't, so it was probed before implementing rather than assumed:
`iosurface-bpr-probe.swift` in this directory, **8/8 HONORED** across widths 1968/1974/1976/1980 at
both 16- and 256-byte alignment. Production then agreed 16/16.

### Not fixed, deliberately

The **256-vs-16 gap itself** is untouched: IOSurface's own default is still 256 and KK still
computes 16. This fix makes the two agree at the one place they meet. A path that allocates an
IOSurface *without* going through `alloc_plain`, and whose bytes are then imported as a linear host
pointer, would reintroduce the same shear — the `[KK-STRIDE]` line exists to make that loud rather
than silent.

## Original repro plan (superseded by the results above)

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
