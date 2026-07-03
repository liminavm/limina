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
(`window::screen_points_for_frame`, queried on the main thread pre-spawn).

Ad-hoc `--window` runs have no bundle: they persist nothing unless given
`--window-state-file <path>`.

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
