# Telling a guest where its display isn't

Status: **exploratory** (2026-08-02). Nothing here is built or scheduled. Companion documents:
`docs/design/display-modes.md` (the shipped `notch = avoid | extend` policy this would eventually
replace the guts of) and `docs/design/stable-edid-hotplug.md` (the EDID synthesis and runtime-push
machinery option A below rides on).

Written after a question with a longer horizon than the current milestone: when kwin and mutter
grow cutout support, will Mac internal displays be describable in whatever mechanism they pick? We
are in an unusual position to influence that, because the thing the ecosystem is missing is the
half we would have to build anyway.

## The problem, stated at the right level

A MacBook's built-in panel has a camera housing that eats a strip of the top edge, and rounded
corners that clip the rest. A limina guest is driven to that panel's geometry and knows none of it.

Our shipped answer is host-side and binary (`display-modes.md`): `notch = avoid` insets the guest
below the housing and leaves a black strip; `notch = extend` gives it the whole panel and lets the
housing sit over whatever lands there, with a deliberate top-edge gesture to get the macOS chrome
back. Both are the compositor's decision made *for* it, from outside, with no way for the guest to
lay out around the obstruction — a top bar cannot dodge the notch it cannot see. The rounded
corners we do not handle at all: under `extend` the guest's corner pixels are simply not visible.

This is not a Mac problem. It is what every *virtual* display faces, and it is why the existing
solutions do not fit us.

## What exists upstream

**`xx-cutouts-v1`** — a Wayland protocol merged into wayland-protocols in 2026 after a two-year
review, authored out of Phosh. The compositor creates an `xx_cutouts_v1` per `xdg_toplevel` and
sends `cutout_box` events (surface-local x/y/w/h, a type enum of `cutout` / `notch` / `waterfall`,
plus an element id so one physical feature can span several boxes) and `cutout_corner` events
(corner + radius). `set_unhandled` lets a client decline specific cutouts so the compositor can
letterbox around those itself. Still in the `xx_` namespace: backward-incompatible changes are
explicitly permitted. Toplevel-only today; layer-surface support is in progress. Implemented by
phoc; **not** by mutter or kwin as far as we can tell.

**gmobile** — the library phoc gets its geometry from. Per-device JSON under
`data/devices/display-panels/`, named `<manufacturer>,<model>.json`, carrying resolution, physical
size, corner radii, and cutouts as SVG paths relative to the panel's top-left. Matched against the
device-tree compatible string in `/sys/firmware/devicetree/base/compatible`. The numbers come from
LineageOS device overlays where they exist, and from someone tracing the shape in Inkscape where
they don't.

Two near neighbours that are *not* this: `wlr-layer-shell`'s exclusive zones are a client
**reserving** space from the compositor (panels), which is the opposite direction and a policy
question; and the "Experimental Zones" protocol that merged around the same time is about window
positioning. The wider prior art is Android's `DisplayCutout` / `layoutInDisplayCutoutMode` and CSS
`env(safe-area-inset-*)`, both visible in the Wayland design.

## The actual gap

`xx-cutouts-v1` is compositor→client, in surface-local coordinates. To send it, a compositor must
already hold the cutout in **output** coordinates and keep it correct across window moves, mode
changes and hotplug. So the standardized half presupposes the half that does not exist: **a way for
a display to say which parts of it are not visible.**

Today that input is out-of-band and static — a database keyed on "which phone am I". For a soldered
panel that is defensible. For us it is unusable on its face:

- our cutout is a property of *which host display the window currently occupies*, and changes on
  hotplug, on dragging to an external monitor, and on a mode change;
- a virtio guest has no stable identity to key a database on, and inventing a DT compatible would
  be keying on the wrong thing anyway — the VM is not the panel;
- gmobile definitions carry `x-res`/`y-res`, so a resize plausibly invalidates the match outright.

Every compositor adopting the protocol will need this input, and absent a standard each will invent
its own answer. That is the piece worth owning, and it is worth stressing that **fixing it for
virtual displays fixes it for real ones too**: a notched laptop panel whose firmware described its
own cutout would not need a hand-curated database entry either.

## Two transports

### A. In the EDID we already synthesize (recommended primary)

A cutout is a fact about the physical panel, which is precisely what EDID exists to describe. We
already generate EDID per display, already emit a **DisplayID 2.0 extension block** (tag `0x70`,
carrying a type VII timing block), and already push a fresh identity at runtime on resize and
hotplug — see `stable-edid-hotplug.md`. Adding a cutout block to that structure is incremental work
in a path that is built, tested (`l1_edid`) and shipping.

Its decisive property under our two-tier rule: **no guest-side change of any kind.** The guest
kernel already fetches EDID over `VIRTIO_GPU_CMD_GET_EDID` and exposes the bytes at
`/sys/class/drm/<conn>/edid`; compositors already parse them via libdisplay-info. A **stock** Fedora
guest on a stock kernel would get cutout data the day its compositor learned to read it. Nothing
about this needs our 16k kernel, our drivers, or our agent.

The cost is that the block has to exist. No standard cutout descriptor was found in a first pass
(see open questions). The bootstrap path is a DisplayID vendor-specific block under our own OUI,
parsed by libdisplay-info, adopted de facto, then taken to VESA if it earns it — which is a slow
road, but the same road every EDID extension travelled.

### B. A DRM connector property

The idiomatic Linux alternative: a blob property on the connector carrying the cutout region,
alongside `EDID`, `PATH`, `non-desktop` and panel orientation. Structured data with no parsing,
and compositors already re-read connector properties on hotplug, so consuming it is nearly free.

**This is the option that needs guest-side changes**, and it needs them in two places on top of the
host work:

| layer | change | tier |
| --- | --- | --- |
| host device | carry cutout geometry in the virtio-gpu display info (libkrun, ours) | — |
| virtio-gpu protocol | a field or command to carry it; a spec change to be upstreamable | — |
| **guest kernel** | `drivers/gpu/drm/virtio` creates the property and attaches the blob | **enhanced only** |
| compositor | read the property, translate to surface-local, emit `xx-cutouts-v1` | — |

The guest kernel patch is what makes it enhanced-tier-only, and therefore what makes it the
*second* choice rather than the first: a stock guest would get nothing, which is exactly the shape
of dependency the two-tier guarantee tells us to avoid when a stock-compatible route exists.

The two are not exclusive, and the honest long-term answer is probably both — EDID as the panel's
self-description, the connector property as the kernel's normalized view of it (fed from EDID when
present, from a device database otherwise). Proposing the property *with* an EDID source already
working is a much stronger position than proposing it alone.

## Why limina is the right place to build this

We are the ideal **reference producer**. We already synthesize EDIDs, drive hotplug, and change
output geometry at runtime; making a cutout follow the host display is the same machinery as
`stable-edid-hotplug.md`, and `hostdisplay::fullscreen_inset` already measures the real inset
empirically per display id rather than trusting the housing height — the exact failure mode a
static database has (43 pt housing vs 44 pt actual inset on dogfood-mac; 32 vs 33 on dev-mac).

We are also a **test rig** for a feature that otherwise requires owning specific phones. A VM that
can produce an arbitrary cutout on demand, change it mid-session, and hotplug it away is worth
something to kwin and mutter developers independently of anything Apple — and it is the kind of
thing that turns "nobody can test this" into a CI job.

And we have a **reference consumer** in reach: `gnome-shell-rs`, the compositor project on the
other side of this work, can implement `xx-cutouts-v1` and take its geometry from us. That closes
the loop end to end in code we control on both sides, which is the demonstration the upstream
conversation would otherwise lack.

## Order of work, if this is ever picked up

1. Research the open questions below. Several could kill or reshape option A.
2. A cutout block in our EDID generator + the host-side geometry (which display, what inset, what
   corner radius), pushed on the existing `DisplayUpdate` path.
3. `gnome-shell-rs` consumes it and emits `xx-cutouts-v1` to clients; that is the end-to-end proof.
4. Only then upstream: libdisplay-info parsing, the connector-property conversation, and the VESA
   question. The Wayland half is already standardized — we would not be proposing a protocol.

## What this does not fix

Not the tier we actually ship. Our enhanced guests run GNOME, and mutter implements neither the
protocol nor any cutout source; `notch = avoid | extend` remains the answer there for the
foreseeable future, and this document does not argue for changing it.

Nor does it touch the chrome ask. That gesture exists because nothing can draw over our
full-panel overlay, which is a macOS-side constraint (`limina-notch-fullscreen`); a guest that
understood cutouts perfectly would still need a way to reach the Mac's menu bar.

## Open questions (all unverified — do not build on these)

- **Is there already a cutout descriptor in EDID/DisplayID?** A first search found nothing, which
  is weak evidence. Check the DisplayID 2.0 spec's block-tag list directly before anything else.
- **DisplayID vendor-specific block mechanics** — tag, OUI registration, and whether
  libdisplay-info and the kernel pass unknown blocks through intact rather than dropping them. The
  kernel silently drops a malformed type VII block whole (`stable-edid-hotplug.md`), so the failure
  mode for a block nobody knows is worth establishing early.
- **Has a DRM cutout property been proposed before?** Search dri-devel before writing a line of it;
  the property being "idiomatic by analogy" is my inference, not observed precedent.
- **Does phoc/gmobile re-read geometry at runtime**, or only at output creation? Determines whether
  a dynamic source is even consumable by the one existing implementation.
- **Does `xx-cutouts-v1` survive its `xx_` phase intact?** Building a guest-facing dependency on an
  explicitly unstable protocol is a cost to time deliberately.
- **Rounded corners on Mac panels**: `cutout_corner` takes a single radius. Whether Apple's corner
  curve is describable that way, and what the actual radius is per model, is unmeasured.

## Sources

- Cutouts protocol reference — <https://wayland.app/protocols/xx-cutouts-v1>
- "The xdg-cutouts-v1 Wayland protocol", Phosh — <https://phosh.mobi/posts/xdg-cutouts/>
- "Avoiding notches", Phosh (gmobile, JSON format, DT-compatible matching) —
  <https://phosh.mobi/posts/notch-support/>
- gmobile — <https://gitlab.gnome.org/guidog/gmobile>
- Android `DisplayCutout` —
  <https://developer.android.com/develop/ui/views/layout/display-cutout>
