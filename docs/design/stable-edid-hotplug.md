# Stable EDID identity + connector events (M15 wave 1, part 1)

**Goal:** the guest's virtual display carries a *real* monitor identity and a *real* mode list,
both derived from the host display the window currently sits on — so GNOME remembers per-monitor
configuration, and moving the limina window from the MacBook panel to an external monitor makes
the guest see that monitor (its refresh rate, its physical size, its identity) instead of one
size-of-the-moment mode that never changes name.

Status: ✅ **SHIPPED and human-verified 2026-07-31.** L1-verified end to end (identity survives a
resize, a pushed identity lands in the guest's `/sys/class/drm/*/edid`, and a pushed disconnect
really moves the connector to `disconnected` and back), then confirmed on two physical displays:
dragging the window moves the guest between two *remembered* monitors and GNOME restores each
one's own resolution and scale in both directions.

**The open design question is answered: the in-place EDID identity swap is sufficient — mutter
re-applies per-monitor configuration without a connector cycle.** The cycle path stays
implemented and L1-tested as the fallback (and for genuine attach/detach later).

Builds on the shipped runtime-resize mechanism
(`docs/design/runtime-display-resize.md`) and its policy layer (`docs/design/display-modes.md`);
this document adds *identity* and *mode list* to what that mechanism pushes, plus the connector
events that make a change legible to the guest compositor.

**End goal this serves** (user, 2026-07-31): the VM follows the host. The near-term case is the
**single window moving between host displays**; "use all displays when fullscreen" (Parallels'
model) and ad-hoc runtime display attach come later, on the same mechanism. Everything here is
designed so the later multi-display step is more scanouts, not a different protocol.

## What the guest actually does with our EDID (verified from source, v7.1 `drivers/gpu/drm/virtio/`)

These four facts determine the whole design. All were read from the kernel tree, not recalled:

1. **The EDID *is* the mode list.** `virtio_gpu_conn_get_modes` (`virtgpu_display.c:182-210`) calls
   `drm_edid_connector_add_modes()` and **returns immediately if that yielded any modes** — the
   host's `GET_DISPLAY_INFO` size is only used as a fallback when the EDID produced nothing. Today's
   single-detailed-timing EDID is why resize works at all: the new size arrives *inside* the EDID.
   Consequence: a richer mode list is purely an EDID change — no protocol or kernel work.
2. **Runtime EDID re-read already works, on the stock tier.** On `VIRTIO_GPU_EVENT_DISPLAY` the
   driver issues `GET_EDID` *and* `GET_DISPLAY_INFO`, then `drm_helper_hpd_irq_event()`
   (`virtgpu_kms.c:46-53`); the response callback runs `drm_edid_connector_update()` and swaps the
   cached blob (`virtgpu_vq.c:906-930`). So pushing a *new EDID* at runtime needs **no kernel
   patch** — the same config-change interrupt libkrun already raises for resize carries it.
3. **A real connector disconnect is one bool.** `virtio_gpu_conn_detect` reports
   `connector_status_disconnected` iff `!output->info.enabled` (`virtgpu_display.c:246-257`), and
   libkrun hardcodes `enabled = true` for every scanout (`gpu/virtio_gpu.rs:2388-2395`). Toggling
   that field *is* hotplug; no new command, no capset.
4. **Trap — only one mode may be PREFERRED, and it must match the pushed rect.**
   `virtio_gpu_conn_mode_valid` (`virtgpu_display.c:213-232`) returns `MODE_BAD` for any mode
   carrying `DRM_MODE_TYPE_PREFERRED` whose size isn't within 16px of `GET_DISPLAY_INFO`'s rect
   (`XRES_DEF`×`YRES_DEF` excepted). Non-preferred modes are always `MODE_OK`. So: **detailed
   timing #0 = the mode we simultaneously push via display-info**, and every additional mode we
   advertise must be non-preferred (standard timings, or later detailed descriptors — `drm_edid`
   only marks the first detailed timing preferred).

## The identity model: identity follows the *host display*

The tension to resolve: "stable identity" and "follow the host" pull opposite ways if identity is
a property of the *virtual* display. It isn't. **A virtual display's identity is the identity of
the host display it is currently presenting on.**

- Stable across resize: dragging the window edge, or the guest re-modesetting, never touches the
  identity fields — only the timing and the physical millimetres.
- Stable across reboots: the identity is derived from the panel's *own* EDID numbers, which
  CoreGraphics reports (`CGDisplayVendorNumber` / `ModelNumber` / `SerialNumber`), so the same
  physical monitor yields the same EDID identity on every boot of every VM. Displays that report
  zeros for all three (virtual and capture displays) fall back to hashing the localized name,
  which is stable and never zero. Known limit: two units of the same model that both report a
  zero serial collide — the same limitation real monitors impose on any compositor.
- Follows the host: moving the window to another display swaps the identity to that display's.
  GNOME then applies *that monitor's* remembered configuration (scale, and later position), which
  is exactly the Parallels behavior asked for.
- Scales to multi-display: when "use all displays when fullscreen" lands, scanout *i* gets host
  display *i*'s identity by the same rule. Nothing about the mapping changes.

Rejected alternative: one fixed identity per virtual display ("limina Display 0") regardless of
host display. It is trivially stable, but it makes every host display look like the same monitor
to GNOME, so per-monitor scale can't differ and the fullscreen-across-displays step would have to
re-litigate identity anyway.

### The EDID fields, and which are identity vs. mode

| EDID field | bytes | class | value |
|---|---|---|---|
| manufacturer id | 8-9 | identity | `LMN` (limina) for limina-driven displays; upstream default stays `RHT` |
| product code | 10-11 | identity | the panel's model number (hashed when it reports none) |
| serial number | 12-15 | identity | hash of the panel's vendor+model+serial numbers |
| week/year | 16-17 | identity | fixed (no build-time drift → byte-stable EDIDs) |
| physical size cm | 21-22 | **mode** | derived from the current mode at constant DPI (see below) |
| detailed timing #0 | 54-71 | **mode** | the current mode, at the host display's refresh — PREFERRED |
| descriptor #1 | 72-89 | identity | product name (`0xFC`) — the host display's localized name |
| descriptor #2 | 90-107 | **mode** | monitor range limits (`0xFD`) — the panel's real refresh range |
| descriptor #3 | 108-125 | **mode** | the alternate detailed timing, else the serial string (`0xFF`) |
| standard timings | 38-53 | **mode** | up to 8 non-preferred fallback modes |

**Physical size stays mode-derived, deliberately.** A real monitor has fixed millimetres; ours
does not, because our window is a *viewport*, not a panel. limina keeps DPI constant (300 by
default) and lets the millimetres track the resolution, so a resize never changes the DPI GNOME
computes and therefore never flips the scale factor mid-drag. Moving to another host display *does*
change the DPI — that is the point, and it changes together with the identity, so GNOME treats it
as the new monitor's property rather than a mode-change surprise.

## When we change what, and which event we raise

Three distinct transitions, three behaviors:

| transition | what changes | event to the guest |
|---|---|---|
| window resize / guest modeset (same display) | detailed timing #0, physical mm | today's path: display-info rect + EDID, one config-change |
| window moves to another host display | identity + range limits + mode list + timing | **disconnect, then reconnect carrying the new EDID** — two config-changes |
| display genuinely removed / added | the scanout exists or not | `connected` toggled, config-change |

The middle row is the load-bearing decision: **a migration is a genuine connector cycle, because
that is the only event both tiers act on.**

An in-place EDID swap — rewriting the identity with the connector left up — is enough for mutter,
which re-reads the connector and re-applies the arriving display's remembered configuration. It is
**not** enough for a compositor that refreshes a monitor's identity only on a reconnect. synoik
keeps reporting the *previous* display's identity after an in-place swap (it re-picks a scale for
the new mode, so it plainly saw the change), and any layout the user then sets is saved under the
wrong monitor — which is how per-display memory is lost across a dock plug/unplug even though every
individual save is internally consistent.

The cycle's cost is what settles it: the guest is at zero monitors for **60 ms**
(`CONNECTOR_DOWN_SETTLE`), and nothing is observable in the window — the user watched both
compositors cycle with the resolution held constant and saw nothing. So the objection that once
chose in-place — that zero monitors is a real session state, with windows relocated, for a
transition the user experiences as dragging a window — does not survive measurement.

That delay is **required, not padding**: the guest has to re-probe while the connector is down, and
back-to-back writes let it coalesce its own re-probe into the end state — picking up the new mode
list and keeping the old identity. 50 ms was the measured floor. Our device layer is not the
constraint (it never merges an update carrying a connection change, and the GPU worker takes one
update per wake, re-kicking its own eventfd while any remain), so what the supervisor owes is order
plus that settle.

Two consequences worth keeping in view:

- **The new EDID rides with the reconnect, atomically**, never with the disconnect. A connector
  that comes back still advertising the old EDID is precisely the stale case.
- **A cycle with no size in the reconnect does not make synoik re-read** (mutter is fine with it).
  That is the shape the `dynamic` and `fixed` modes send, since neither may dictate the guest's
  resolution. `host` — the default — folds the size in and works on both. Closing that gap is
  synoik's side; limina must not start driving the resolution in modes that promise not to.

`LIMINA_DISPLAY_HOTPLUG=inplace` selects the in-place swap. It stays supported because it is the
cheaper event where a guest honours it; unset or unrecognized means the cycle.

## As built

Four layers, all landed:

| layer | where | what |
|---|---|---|
| EDID generator | libkrun `0119` (`gpu/edid.rs`, `gpu/display.rs`) | optional identity / standard timings / second detailed timing / range limits; defaults unchanged apart from the digital-input fix below |
| runtime push | libkrun `0120` (`gpu/{device,worker,virtio_gpu}.rs`) | `DisplayUpdate {size, edid, connected}` queue; `enabled` now drives connector status |
| wire format | `crates/limina-displayctl` | `resize W H` (unchanged) + `display id=… …`; dependency-free so both sides share one definition |
| host policy | `crates/limina/src/window/hostdisplay.rs` + the match-host tracker | host display → identity/DPI/refresh/VRR range; pushes on migration |

Three things came out differently from the plan, all for reasons worth keeping:

- **The screen tracker had to be re-keyed on identity, not size.** It compared screen *sizes*,
  so a move between two same-sized displays pushed nothing at all. It now carries an identity
  key (serial+product+refresh) alongside the size.
- **DPI is derived now, and that is a behavior change.** limina advertised a flat 300 DPI to
  every display, which made an ordinary external monitor look Retina to the guest and pushed it
  to a 2× scale. It is now the panel's real point density × the backing scale factor: a Retina
  panel still lands ~254 DPI (so the guest keeps the 2× it has today), while a 27" 1440p
  external now correctly reports ~109 DPI and gets 1×. Displays that report no physical size
  (projectors, some capture devices) fall back to the old 300, so nothing that worked before
  can regress.
- **The EDID claimed to be an *analog* display.** Byte 20 (video input definition) was left
  zero, which every parser reads as analog — visible in `monitor-edid`'s output as "Analog
  signal" for a display that has never been anything but digital. It now declares digital,
  8 bits per colour, DisplayPort. This is the one respect in which the default output is no
  longer byte-identical to what libkrun produced before.
- **macOS display names don't fit.** The EDID name field is 13 bytes and "Built-in Retina
  Display" hard-truncated to `Built-in Reti` in the guest's monitor list. Over-long names are
  now cut at a word boundary when there is one in the second half of the field.
- **The serial-string descriptor is the one that gets dropped.** A base EDID block holds four
  descriptors; a ProMotion panel wants five (timing, name, range, alternate timing, serial
  string). Priority order drops the serial string — the numeric serial in bytes 12-15 already
  carries the identity — and the generator logs when it does.

## What is not done

- **Boot-time EDID.** The identity is pushed at runtime, on the first poll after the guest
  presents a frame — so the guest briefly sees the anonymous `krun-display` identity first. In
  practice that first frame is EFI/GRUB output, long before a compositor starts, so nothing
  observes the change. Plumbing the identity through the worker's boot arguments would remove
  the window entirely.
- **A real mode list.** `modes` is wired end to end but the supervisor sends none: the
  standard-timing encoding can't express most real Mac point sizes (widths must be a multiple
  of 8 below 2288, in four aspect ratios). The DisplayID extension block (below) is the vehicle;
  it currently carries only the timings the base block cannot express.
- **VRR itself.** The range descriptor now reaches the guest in the exact form
  `drm_get_monitor_range` accepts, which is the prerequisite — but virtio-gpu has no
  `vrr_capable` plumbing at all. That is protocol + kernel work, tracked with the M15 wave-2
  extension.

## The DisplayID extension block, and HiDPI

**Correction to an earlier note in this doc: a CTA-861 extension would not have helped.** Its
detailed timings are the same 18-byte descriptor as the base block's, with the same 16-bit pixel
clock in 10 kHz steps — the same 655.35 MHz ceiling. The vehicle that lifts it is **DisplayID 2.0**
(EDID extension tag `0x70`), whose **type VII** timing block (tag `0x22`) stores a 24-bit clock in
**kHz**. Verified against `drm_displayid.c` and `drm_mode_displayid_detailed` in `drm_edid.c`, not
recalled.

The base block cannot simply stop carrying an over-ceiling mode: `virtio_gpu_conn_mode_valid`
prunes any *preferred* mode whose active size differs from the rect pushed through
`GET_DISPLAY_INFO`, so the preferred timing has to stay at the size the guest is driven to. So the
generator emits both — the base detailed timing at the driven size with a clamped refresh, and a
DisplayID type VII timing at the same size with the real one, flagged preferred. Same active size,
so it survives the same pruning check; `drm_mode_sort` orders equal-priority modes by descending
clock, so the honest rate lands first. Only over-ceiling timings move; a mode the base block can
express produces no extension at all, byte for byte as before.

Parser rules that the encoder has to satisfy (all asserted by the tests, which decode the block
independently rather than calling back into the encoder):

- The DisplayID structure spans extension bytes `[1, 127)` — the kernel says outright that "EDID
  extensions block checksum isn't for us" — and carries **its own** checksum covering the 4-byte
  header, every block, and itself.
- A type VII block whose `num_bytes` is not a multiple of 20 is **dropped whole**, silently.
- Every field in a timing is stored as `value - 1`; the sync fields carry polarity in bit 15.
- One 128-byte extension therefore holds at most five timings.

**HiDPI** (`[display] hidpi`, default on; `--no-hidpi`) is what makes this matter. ✅ **Human-verified
2026-07-31** on the 14" Retina panel: the guest is driven at 3024×1964, the desktop is sharp rather
than soft, and GNOME offers the 200% scale it previously could not. A guest pixel is
now a *device* pixel rather than a point: on a 2× panel the guest is driven to 3024×1964 instead of
1512×982, renders at the panel's native resolution, and — given the density we report — picks the
2× scale itself, instead of rendering at half resolution for Core Animation to upscale. It costs
4× the guest framebuffer and 4× the fill, which is what the opt-out is for.

The conversion lives in exactly one place, `window::fit::Scale`, and only four sites cross the two
unit systems: the boot-time size in `main.rs`, `hostdisplay::describe`, the dynamic-mode
window→guest push, and the dynamic-mode guest→window follow. Everything else — the letterbox fit,
the pointer mapping, the layer frame — stays in points and needed no change, because CA maps the
larger surface onto the same point-sized layer 1:1 in device pixels.

Two consequences worth recording:

- The advertised **density is the panel's**, not the framebuffer's, in both modes: it describes the
  glass. Under HiDPI that happens to also be the framebuffer's density, which is why GNOME then
  offers 2×.
- The range descriptor's horizontal-rate fields are bytes, and 4K at 120 Hz is 265 kHz. EDID 1.4's
  **+255 offset flags** (byte 4, bits 2/3) carry the excess; understating the bound would have the
  guest prune the very mode we advertise.

### An L1 limitation worth remembering

`l1_edid` asserts the extension arrives byte for byte and satisfies every framing rule the kernel
checks — both checksums, the `0x70`/`0x22` tags, the multiple-of-20 length — but **not** that the
guest builds a mode from it. The minimal L1 guest does not surface large detailed timings in
`<connector>/modes` at all: a 2560×1440 @ 60 Hz push, comfortably inside the base block's clock
ceiling and touching none of the extension code, collapses the list the same way. So that is a
property of the L1 vehicle, not of the extension, and the end-to-end mode selection is verified on
a real desktop guest instead. Worth chasing separately if the L1 vehicle ever needs to assert on
mode lists.

## Layers

**Layer 1 — libkrun EDID generator** (`src/devices/src/virtio/gpu/edid.rs`, new patch).
`EdidParams` grows an optional identity (manufacturer, product, serial, name, serial string), a
mode list, and a refresh range; all default to today's values so upstream behavior is unchanged
when nothing is set. Unit-tested against an independent decoder in the same patch (checksum,
descriptor tags, decoded timings) — these are upstreamable as-is.

**Layer 2 — libkrun runtime update** (`gpu/{device,worker,virtio_gpu}.rs`, new patch).
`DisplayResizeHandle` generalizes to carry `(display_id, width, height, Option<EdidParams>,
enabled)`; the worker applies it to `displays[id]` and raises the config-change exactly as the
resize path does today. `request(id,w,h)` stays as a thin resize-only wrapper so nothing that
exists breaks. Mechanism only — *which* identity, and when, is limina's.

**Layer 3 — limina-vmm control socket.** The display-control socket learns a `display` verb
carrying the full update alongside today's `resize W H` (which keeps working).

**Layer 4 — limina host policy** (`crates/limina/src/window/`). The host-mode screen tracker
today keys on screen *size* (`screen_sent`), so a move between two same-sized displays pushes
nothing. It re-keys on the host display's identity, and the push carries the new display's
identity, native refresh, DPI and mode list.

## Two-tier posture

Everything here rides mechanisms a **stock** guest already implements (facts 1-3 are upstream
kernel behavior, present since the `drm_edid` conversion). A stock guest gets stable identity,
the real mode list and the host-follow behavior with no limina guest components. What the
enhanced tier adds later is VRR (`vrr_capable` has no virtio-gpu plumbing at all — that is a
protocol + kernel item, tracked with the M15 wave-2 extension, not here) and >1 scanout policy.

## RED-first tests

- **Unit (libkrun):** decode-back tests over the generator — identity bytes are byte-identical
  across two different modes; the mode list round-trips; exactly one detailed timing is preferred;
  checksum valid; the upstream default output is unchanged.
- **L1 (`crates/limina-test/tests/l1_edid.rs`):** boot the tiny guest with the console shell, read
  `/sys/class/drm/<conn>/edid`, resize, and assert the identity bytes are unchanged while the
  timing moved; then push an identity change and assert the new identity lands in sysfs; then
  exercise the `cycle` path and assert `status` goes `disconnected` → `connected`.
- **Human (windowed):** drag the window between the built-in panel and an external monitor;
  confirm GNOME reports the right monitor name/refresh and keeps per-monitor scale.

## Map citations (point-in-time; re-verify before editing)

- Guest kernel v7.1: `virtgpu_display.c:182-210` (get_modes), `:213-232` (mode_valid),
  `:246-257` (detect); `virtgpu_kms.c:36-58` (config-changed work); `virtgpu_vq.c:819-845`
  (display-info cb + hotplug event), `:894-930` (EDID cb + `drm_edid_connector_update`).
- libkrun: `gpu/edid.rs` (generator, 316 lines); `gpu/display.rs:8-24` (`DisplayInfo`,
  `edid_bytes`), `:32-50` (`EdidParams`, `PhysicalSize`); `gpu/virtio_gpu.rs:2388-2395`
  (`display_info`, hardcoded `enabled = true`), `:2398-2404` (`get_edid`), `:2406-2420`
  (`set_display_size`); `gpu/device.rs:52-92` (`DisplayResizeHandle`); `gpu/worker.rs:633-648`
  (apply + config-change).
- limina: `limina-vmm/src/krun/mod.rs:707-760` (control-socket listener, `parse_resize`);
  `limina/src/window/mod.rs:107-135` (`screen_info_for_frame`), `:608-612` (`screen_sent`).
