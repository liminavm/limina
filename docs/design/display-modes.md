# Display modes: match-host / dynamic / fixed + remembered window state

Status: SHIPPED 2026-07-03. Builds on the runtime display-resize mechanism
(`docs/design/runtime-display-resize.md`) — this document is the *policy* layer on top of
that shipped mechanism; nothing here touches the worker or libkrun.

## Why

The guest used to drive the host window: every guest modeset yanked the NSWindow to the
new size via `setContentSize` (boot's EFI → GDM → desktop ladder, or an in-guest xrandr),
which was jarring, and the window forgot its frame across runs. The fix: the presented
display becomes a per-VM policy, and only one of the three policies lets the guest move
the window.

## The one config key

`vm.toml`:

```toml
[display]
resolution = "host"        # DEFAULT — or "dynamic", or "1920x1080"
```

CLI: `--display-resolution <host|dynamic|WxH>` (ad-hoc runs), `limina start
--display-resolution …` (one-shot override). One key, three shapes
(`vmlib::schema::DisplayResolution`), so no invalid mode/size combos exist. Fixed sizes
below 64×64 are rejected at parse (the runtime resize floor).

**Behavior change, intentional:** configs that predate the key (it is `#[serde(default)]`)
gain match-host on their next start — the default is `host`, not the old dynamic behavior.

## The mode matrix

| | initial `--display-size` | guest modeset → window | pushes → guest | letterbox |
|---|---|---|---|---|
| **host** (default) | point size of the target screen | never — letterbox absorbs it | screen size, on screen change only | when guest ≠ view |
| **dynamic** | remembered content size, else `--display-size` (1280x800) | `setContentSize` (the original behavior) | window size, on drag-end (unchanged guards) | never (fit ≡ full view) |
| **fixed WxH** | the configured WxH | never | never | when guest ≠ view |

- Everything is supervisor-side policy over two shipped mechanisms: the worker's
  `--display-size` boot argument, and the `resize W H` display-control socket →
  `DisplayResizeHandle` (libkrun patches 0025–0027).
- Points ⇄ guest pixels stays 1:1 (the shipped resize convention); `backingScaleFactor` /
  HiDPI is deferred. "Screen size" is `NSScreen.frame` in points.
- **Fullscreen in host mode needs no modeset** — the guest already runs at screen
  resolution; the bars just vanish. That is the headline UX win of the default.
- Two-tier posture: a stock guest that ignores the display-info config-change interrupt
  stays at its (mode-correct) boot resolution and letterboxes — degraded, never broken.

## Letterboxing (host/fixed)

The scanout layer is aspect-fit into the content view (`window/fit.rs` — pure math, unit
tested); the uncovered margin is the window background, set black. The **same** `FitRect`
is shared (an `Rc<Cell<_>>`, main-thread only) with the input path: `abs_coords` maps
through it (offset + scale + clamp) and `pointer_inside` gates on it, so the pointer can
never disagree with the pixels. Clicks/scroll on the bars are ignored (like window
chrome); drags that leave the content clamp to its edge; the relative/captured pointer
path is untouched. The capture-mode cursor overlay composites relative to the scanout
layer's own bounds, so it letterboxes for free.

In dynamic mode the fit is pinned to the full view every tick, which makes the transform
bit-identical to the legacy full-bounds mapping (unit-proven in `fit.rs`).

## Window aspect lock (host/fixed)

`window/mod.rs::apply_aspect_lock` sets `NSWindow.setContentAspectRatio` so **interactive**
resize is constrained to the guest's aspect: host mode → the display's aspect (re-applied on
display migration, alongside the screen-size push), fixed mode → the configured WxH, dynamic →
unconstrained (the guest follows the window there, so a free resize is the point). This stops
the user from dragging the window into a shape the guest never fills — which would only grow
the letterbox bars. `setContentAspectRatio` bounds *user* resizing (and the zoom button) only;
it never resizes the window itself, so the boot frame and any restored frame are left as-is and
the letterbox still absorbs any residual mismatch until the next drag. Fullscreen ignores it
(the screen already matches the locked aspect in host mode).

**Display migration reshape (host).** `setContentAspectRatio` is a constraint on *future*
interactive resize, not a reshape — so on its own it lets the window's actual shape drift from
the locked aspect. When the user drags a host-mode window to a differently-shaped display,
AppKit keeps the window at its old shape while we drive the guest to the new screen's aspect, so
the guest stops filling the window → a letterbox appears until the user manually resizes. The
migration branch therefore also `setContentSize`s the window to the new aspect immediately, via
`fit::reshape_to_aspect` — which **preserves the window's on-screen area** (trades width for
height, so it neither balloons nor collapses) and clamps into the new screen's visible frame.
The per-tick fit recompute then re-fits the scanout with no bars. No feedback loop: host-mode
window resizes push zero modesets, and the guest re-push is keyed off the *screen* size, not the
window size, so the reshape can't re-trigger a push.

## Firmware / GRUB run at a modest resolution (upscaled), not the full screen

On the EFI path in host mode the guest boots at the screen size (e.g. 2560×1440), so **the
firmware console and GRUB** would draw a small, fixed-size menu centered in that huge
framebuffer, padding the rest with black — a "letterbox" *inside the guest scanout* that the
compositor cannot upscale (it can't tell the menu from GRUB's own black pixels). Root cause:
`OvmfPkg/VirtioGpuDxe`'s `GopInitialize` overwrites the modest `PcdVideoHorizontalResolution|1280`
/ `…VerticalResolution|800` the `.dsc` sets with the host's native display-info size whenever
`PcdVideoResolutionSource == 0`. The fix (`scripts/build-krun-efi.sh`) pins
`gUefiOvmfPkgTokenSpaceGuid.PcdVideoResolutionSource|1` — the same sentinel OvmfPkg's Setup uses
when the user picks a resolution — so the driver leaves the modest 1280×800 alone. Firmware/GRUB
then render at 1280×800 and the host aspect-fits/**upscales** them to fill the window; the guest
**kernel** is unaffected (virtio-gpu-drm queries display-info directly and still modesets to the
full screen for a crisp desktop). Host-side and tier-agnostic — stock and enhanced both benefit.
Verified by the scanout-geometry timeline (firmware/GRUB `640×480 → 1280×800`, kernel
`→ 2560×1440`) plus a human eyeball on the enlarged menu.

## Push triggers (why they differ per mode)

- **dynamic** keeps the shipped drag-end debounce verbatim: push once the drag settles
  (`inLiveResize` gate), guarded by `geom` (the guest's current resolution — kills the
  `setContentSize` echo) and `resize_sent` (dedup).
- **host** polls `window.screen()` in the 60 Hz timer (the codebase's deliberate
  no-NSWindowDelegate pattern) and pushes when the screen's point size differs from the
  last one driven (`screen_sent`, seeded with the boot size so startup pushes nothing).
  Window drags/resizes never modeset the guest. A guest-side xrandr is respected
  (letterboxed) until the next screen change re-asserts.
- **fixed** never pushes; the boot `--display-size` carries the resolution. The
  display-control socket stays bound (the test harness can still drive it).

Every push also records itself in a shared `desired_size` (`session::pack_size`) that the
reboot-relaunch monitor reads, so a worker relaunched across a guest reboot boots at the
*current* resolution — required now that host/fixed windows are never rescued by
`setContentSize`.

## Remembered window state

`<bundle>/state.toml` (`vmlib/state.rs`), written atomically (tmp+rename), deliberately
disposable — missing/corrupt reads as "no state", deleting it just forgets the placement:

```toml
[window]
frame = [x, y, w, h]   # NSWindow frame, screen points, Cocoa bottom-left origin
content = [w, h]       # content size in points — dynamic mode's remembered resolution
```

Why not the alternatives: NSUserDefaults frame-autosave doesn't travel with the
Finder-copyable bundle and can't be read (sanely) before the window exists — and the
supervisor must know the remembered size *before spawning the worker* to derive
`--display-size`; a vm.toml key would make every window drag rewrite user-editable config.

Saved from the render timer once the frame settles (~0.5 s stable, not mid-drag, never
the fullscreen frame — the windowed frame is what we remember), off-thread; plus a final
synchronous save on the quit paths. Restored at window creation when the frame still
intersects a live screen (else `center()`); in host mode the *screen holding the
remembered frame's midpoint* is the one whose size seeds the boot resolution
(`window::screen_info_for_frame`, queried on the main thread pre-spawn).

## First-appearance default size

With no remembered frame, the window opens at **half the display's area, at the guest's
aspect ratio**, clamped into the visible frame (`fit::default_window_content`) — never
tiny, never screen-filling, and bar-free on first show because the content aspect
matches the guest. Dynamic mode derives its first-boot *guest* resolution from the same
rule at the screen's aspect (window == guest, no early re-modeset); `--display-size` is
only the last-resort fallback on a screen-less host. Fixed mode keeps the configured
aspect, so a 4:3 fixed guest gets a 4:3 half-area window.

Ad-hoc `--window` runs have no bundle: they persist nothing unless given
`--window-state-file <path>`.

## Fullscreen on a notched display (`[display] notch`, 2026-08-01)

On a MacBook Pro/Air built-in panel the camera housing splits the top of the screen, and macOS
will not let a **Space** draw beside it. That is the finding, and it took two rounds to get right;
the full measurement is in `spikes/notch-fullscreen/RESULTS.md`.

Things that look like levers and are not:

- `setFrame(screen.frame)` while fullscreen — ignored, AppKit re-imposes its own height.
- `window(_:willUseFullScreenContentSize:)` — consulted, then overridden.
- `NSPrefersDisplaySafeAreaCompatibilityMode = true` — **the trap.** It really does hand the
  fullscreen window a frame covering the whole panel and really does zero `safeAreaInsets`, and
  the compositor masks the housing strip anyway. It buys no pixels and destroys the only signal
  that would have said so. It is not a fullscreen key at all: compatibility mode is the system
  changing the display's *active area*, triggered by "an app that requires it plac[ing] a window
  behind the camera housing". We ship it **`false`** — present, not omitted, because panel
  fullscreen is exactly that trigger and `false` is what says *never*, while omitting it leaves
  the decision to a Finder Get Info checkbox we do not control.
- `.titled` + `.fullSizeContentView` at `screen.frame` — also masked. The mask keys on the style
  mask, not just the frame.

Apple documents the same conclusion from the other side, which is worth knowing before anyone
tries again: `NSScreen.safeAreaInsets` says that when you call `toggleFullScreen(_:)` "the system
automatically positions the window's contents within the safe area", and offers no way to decline
— it names "a custom full-screen experience" as the alternative. The HIG agrees that the system
full-screen support "automatically accommodates" the housing.

That custom experience is what works — but a borderless window on its own is **not a Space**, so
it costs Mission Control, the swipe and the fullscreen animation. The shipped design keeps both:
**native fullscreen as a carrier, with the borderless window floating over it** as a
`.fullScreenAuxiliary` window above menu-bar level (`ExtendOverlay`). Measured (round 5): the
overlay paints beside the housing, takes keyboard focus, and the menu bar cannot appear over it.

| `[display] notch` | fullscreen | guest gets | the strip |
|---|---|---|---|
| `avoid` (default) | native, AppKit insets it | the safe area | black |
| `extend` | native carrier + full-panel overlay | the whole panel | guest content, housing overlapping it |

Both policies use `toggleFullScreen:`, so the green title-bar button and Cmd-Ctrl-F finally do the
same thing.

### What the overlay costs, and the four rules that pay for it

The overlay gets **no view of its own**: the same `NSView` — scanout layer, cursor sublayer, every
input binding — is re-parented into it and back out, so nothing that holds the view knows it moved.
Sizing reads `ExtendOverlay::active_view` rather than the carrier's content view.

It is up only while *all* of these hold, each for its own reason:

- **The carrier is natively fullscreen.** The overlay needs a Space to float over.
- **The screen has a camera housing.** On an external display native fullscreen already covers
  everything, so an overlay would be risk for no pixels.
The overlay's *level* is separate from whether it is up: `OVERLAY_LEVEL` (above the menu bar)
while limina is active, ordinary otherwise. Height above the menu bar is only worth anything while
the pointer is in the guest, and it costs something real — a window at that level covers **system**
windows too, and it hid the Accessibility grant dialog behind the very app that needed the grant.

- **The carrier's Space is the one on screen** (`isOnActiveSpace`). A window above menu-bar level
  would otherwise float over whatever the user Cmd-Tabbed to. Dropping it returns the view to the
  carrier, so the Space still shows the guest — the right look for a background app anyway.
  Deliberately *not* `isActive`: activating an app on another display leaves limina's Space
  perfectly visible on its own, and the first cut shrank a still-on-screen guest back below the
  housing for no reason. Activation says which app has the keyboard; the overlay's question is
  whether its Space is showing.
- **The user is not asking for the chrome.** Nothing can reveal over the overlay, which is the
  point of it; but the menu bar and the window's own controls still have to be reachable for the
  VM's menu actions. A deliberate shove at the top edge — **uncaptured only**, so a grabbed pointer
  can never trip it — puts the overlay down until the pointer returns to the guest, `REVEAL_MARGIN`
  (40 pt) clear of the top.

  The ask has **one** implementation, `InputState::reveal_step`, which both input paths call — the
  local monitor directly, and the capture tap in place of its own resistance breakthrough. It must
  work with *and* without the Accessibility grant (any freshly compiled binary lacks it, since TCC
  keys on the code hash), and those are different code paths, so a single owner is the only way
  they can agree. It briefly had two, and reworking one silently left the other in force: with the
  tap installed it consumes the edge events, so the monitor's version never ran at all.

  Ways to get this wrong, each found by dogfooding it:
  - **The release must not be gated on the overlay being up.** It was, and the overlay is down
    *precisely because* the ask is set — an unreachable state, so the guest stayed inset below the
    housing forever, across fullscreen toggles included. The release condition has to be live at
    all times. (Leaving fullscreen clears the ask outright too, so entering it always starts from
    the overlay.)
  - **Distance is the wrong currency.** It rewards a hard shove, and a hard shove is exactly what
    throwing the pointer at the top-left hot corner looks like, so the menu bar kept appearing
    while the user reached for the GNOME overview.
  - **Wall-clock runs through silence.** Resting against the top produces no events at all, so a
    quick shove followed by a linger satisfied a "hold" nobody performed.
  - **An unbroken hold is a *mouse* gesture.** A trackpad stroke ends when the finger runs out of
    glass; every lift reset the run and the chrome became unsummonable on the hardware the app is
    actually used on.

  What it settles on: a **charge** — time actually spent pushing, capped per event so silence
  cannot be banked as motion, decaying after `REVEAL_DECAY` of idle, with a small distance floor
  to rule out jitter. Repeated strokes add up; a single shove does not. And it never arms within
  `fit::CORNER_ZONE` of a side edge, because corners belong to the guest.

Two knock-on simplifications. `NSWindowStyleMask::FullScreen` is meaningful again, since fullscreen
is always native; only the capture tap needs the overlay flag, because a guest hosted in the
overlay is fullscreen for resistance purposes while the overlay carries no fullscreen bit. And the
resistance **keep-out band is switched off** under the overlay: it exists only because macOS
reveals chrome on contact, and holding the cursor two points short would otherwise stop the guest's
top bar and top-left hot corner getting the pointer at the true corner.

Because a borderless `NSWindow` refuses key status — and the overlay carries the guest — the window
is a `LiminaWindow`, an `NSWindow` subclass overriding
`canBecomeKeyWindow`/`canBecomeMainWindow`, from creation.

### Sizing the guest

`hostdisplay::describe` decides the guest's resolution from `screen.frame` minus the policy's
inset — always, not only while fullscreen, so entering fullscreen never modesets. Under `extend`
the inset is zero (the panel-fullscreen window *is* the panel). Under `avoid` it is
`hostdisplay::fullscreen_inset`, which is subtly **not** the housing height: the housing measures
43 pt on dogfood-mac and 32 pt on dev-mac while the native fullscreen window comes up 44 and 33 pt
shorter. Rather than fit a constant to two data points, the real figure is observed the first time
the window is natively fullscreen on a display and cached against its id; until then the housing
height stands in, at the cost of one 1 pt modeset. Being exact matters because the guest is driven
to this number — a point of error rescales every frame by 0.08% instead of landing 1:1.

The per-tick fit subtracts **nothing**. Under `avoid` AppKit has already inset the content view;
under `extend` every point of it is ours. (An earlier cut subtracted the housing here too, which
double-counted it once the plist key came out.)

## Fullscreen edge resistance (`[display] edge-resistance`, 2026-08-01)

In fullscreen the host cursor touching the top edge instantly reveals the macOS menu bar and title
bar, and a side edge silently hands the pointer to the next display. Both are one flick away, which
makes a fullscreen guest feel leaky — the Parallels behavior is that leaving takes a deliberate
push. `fit::EdgeResist` implements that: outward motion at an edge is absorbed until the
accumulated push crosses the threshold (default 100 pt), then the pointer breaks through and stays
free until it re-enters the content. The absorbed motion is forwarded to the guest's relative-mouse
device as edge pressure — the same `send_edge_overflow` captured mode uses — so mutter's barriers
(GNOME hot corner, a guest panel's reveal edge) still fire while the cursor is held.

The accumulator drains when the pointer retreats more than `RELEASE_MARGIN` back inside the held
edge, so sliding *along* an edge never adds up to a breakthrough and two nudges separated by real
inward motion don't either.

Mechanically this lives in the capture tap's uncaptured path (`capture_tap::resist_edges`): the
motion event is consumed and the host cursor warped back inside the boundary, and the warp's own
event is what re-drives the guest's pointer. It therefore needs the same Accessibility grant as
pointer capture, and applies **only** while fullscreen and key — windowed, the pointer must be free
to leave. `edge-resistance = 0` disables it; pointer capture (Cmd-Ctrl-G) remains the absolute
version, parking the host cursor at screen centre.

### Three things the first cut got wrong (dogfood, 2026-08-01)

Worth keeping, because each is a trap the obvious implementation walks into.

**Pinning the cursor *on* the top edge does not stop the chrome.** macOS reveals the menu bar and
title bar the moment the cursor touches the top row of the screen — the reveal has already happened
by the time resistance could push back, so the top edge felt like it had no resistance at all while
the sides felt strong. The fix is a `KEEPOUT` band (2 pt): while resistance holds, the cursor parks
*short* of the edge on all four sides, so the trigger is never touched (the Dock's auto-hide reveal
on whichever edge it lives gets the same protection for free). The guest doesn't notice, because
while we hold, its pointer is driven by the forwarded pressure rather than by the host cursor's
position. Related: the top and bottom hold the full threshold while the sides take `SIDE_FACTOR`
(half) of it — crossing to another display is a thing you mean to do; macOS chrome dropping over
the guest never is.

**A corner is a target, not an exit — and it never releases.** mutter's pressure barriers (the
GNOME hot corner among them) want ~100 px of accumulated push, and we forward exactly the motion we
absorb; at the plain side threshold we let the pointer go after 50 pt, so the corner charged
part-way and stopped. Raising the corner's threshold was the right diagnosis at the wrong layer:
whatever the multiple, *breaking through ends the pressure*, and a barrier wanting sustained motion
cannot be served by a bounded burst. Within `CORNER_ZONE` (32 pt) of a corner the pointer is
therefore held indefinitely, so the push can continue as long as the user pushes. Nothing is
trapped — sliding a few points along either edge leaves the zone and the ordinary thresholds apply.

**While resisting, pin the guest's pointer to the clamped position** (`send_abs_position`). The
local monitor is the only other thing driving the absolute device and it never runs while the tap
consumes, so the guest's cursor sat tens of points short of the corner and spent the first part of
the forwarded push *travelling* there rather than charging the barrier: 142 px measured arriving as
~90 px of pressure, just under the 100 needed. That was the entire "works one time in three".

**A warp opens a 0.25 s local-events suppression interval.** During it, real mouse movement stops
moving the cursor. Resistance warps on every held event, so pushing at an edge and then coming back
felt like the barrier had *eaten* the travel — you had to move a long way before the cursor
unstuck. `CGAssociateMouseAndMouseCursorPosition(true)` immediately after the warp ends the
interval (`input::end_warp_suppression`); it is a no-op for association itself, since the
uncaptured path is associated by definition.

**Do not integrate the position yourself.** The first cut advanced a position kept by the window's
own motion handler — which sees nothing while the tap is consuming events, and nothing *at all*
once the pointer has left the window for another display. It froze on the edge, so small moves made
over on the neighbouring display computed as re-entering the content, re-armed the resistance, and
warped the pointer home: "sometimes the mouse crosses to a separate workspace and then gets thrown
back". `EdgeResist::step` now takes the event's own `CGEventGetLocation` (via
`input::cg_global_to_view_point`, which happily answers with coordinates outside the view) and uses
the deltas only for pressure — which is the right split anyway, since at a real screen edge the
position stops changing while the deltas keep coming. Re-arming likewise needs a genuine retreat
inside the content, not mere boundary contact: once through the top, the cursor rests *on* the
content's own top boundary, and a boundary-inclusive test re-arms instantly and drags the pointer
off the menu bar it just earned.

## Files

`crates/limina/src/window/fit.rs` (pure letterbox math + inverse input mapping),
`crates/limina/src/vmlib/state.rs` (state.toml), `vmlib/schema.rs`
(`DisplayResolution`), `main.rs` (`--display-resolution`, `initial_display_size`
derivation), `session.rs` (`WindowOptions` plumbing + `desired_size` relaunch),
`window/mod.rs` (mode-gated apply/push/persist), `window/input.rs` (fit-rect transform).

## Verification

Unit: fit math, schema serde, state round-trip, derivation table (`cargo test -p
limina`). The capture/headless path (`--display-capture`, the whole limina-test harness)
is untouched by design — `scripts/test-boot.sh` is the regression gate. Windowed behavior
(letterbox pixels, pointer accuracy, fullscreen, screen migration, restore-on-relaunch)
needs the human eyeball pass via `spikes/venus-draw-probe/boot-seated-kk.sh`.
