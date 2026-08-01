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

On a MacBook Pro/Air built-in panel the camera housing splits the top of the screen. AppKit's
default is to inset a fullscreen window **below** it: measured on a `1512x982` panel, the
fullscreen window is `1512x949` and that 33 pt strip is unusable. Two apparent levers are not
levers — `setFrame(screen.frame)` while fullscreen is ignored, and
`window(_:willUseFullScreenContentSize:)` is consulted and then overridden. The only thing that
changes it is the bundle's `NSPrefersDisplaySafeAreaCompatibilityMode`, which reads backwards:
**`true` gives the fullscreen window the whole panel.** Full A/B in
`spikes/notch-fullscreen/RESULTS.md`.

So `scripts/build-app.sh` sets that key and limina owns the policy per VM:

| `[display] notch` | fullscreen guest | the strip |
|---|---|---|
| `avoid` (default) | inset below the housing | black |
| `extend` | the full panel | guest content, housing overlapping |

`fit::usable_content` does the trim; `hostdisplay::notch_inset` supplies the height. That height
has to be **cached per display**: under the compatibility key AppKit reports `safeAreaInsets` and
`auxiliaryTopLeftArea` only while the window is *not* fullscreen, which is exactly when the policy
needs them.

The inset is applied in two places, deliberately at different times. `hostdisplay::describe`
subtracts it **always**, so the guest's resolution already matches what fullscreen will hand it
and entering fullscreen never modesets. The per-tick fit subtracts it **only while fullscreen**,
because a windowed window is never under the housing. This also fixed a latent bug: host mode used
to drive the guest to the full `screen.frame` while the fullscreen view was 33 pt shorter, so
`aspect_fit` letterboxed the guest on all four sides.

Caveat: the key is in the app bundle, so a bare `target/debug/limina` (what `cargo xtask run`
launches) still gets the 949 pt window. `avoid` looks identical there; `extend` must be validated
from a built `Limina.app`.

## Fullscreen edge resistance (`[display] edge-resistance`, 2026-08-01)

In fullscreen the host cursor touching the top edge instantly reveals the macOS menu bar and title
bar, and a side edge silently hands the pointer to the next display. Both are one flick away, which
makes a fullscreen guest feel leaky — the Parallels behavior is that leaving takes a deliberate
push. `fit::EdgeResist` implements that: outward motion at an edge is absorbed until the
accumulated push crosses the threshold (default 100 pt), then the pointer breaks through and stays
free until it re-enters the content. The absorbed motion is forwarded to the guest's relative-mouse
device as edge pressure — the same `send_edge_overflow` captured mode uses — so mutter's barriers
(GNOME hot corner, a guest panel's reveal edge) still fire while the cursor is held.

The accumulator drains on any event that isn't pushing outward, so sliding *along* an edge never
adds up to a breakthrough and two nudges separated by inward motion don't either.

Mechanically this lives in the capture tap's uncaptured path (`capture_tap::resist_edges`): the
motion event is consumed and the host cursor warped back to the boundary, and the warp's own event
is what re-drives the guest's pointer. It therefore needs the same Accessibility grant as pointer
capture, and applies **only** while fullscreen and key — windowed, the pointer must be free to
leave. `edge-resistance = 0` disables it; pointer capture (Cmd-Ctrl-G) remains the absolute
version, parking the host cursor at screen centre.

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
