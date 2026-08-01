# Notch / safe-area behavior of a fullscreen AppKit window — measured

**Date:** 2026-08-01
**Host:** macOS 26.5, M1 Max. Built-in Retina Display `1512x982 @ 2x`, external BenQ `2560x1440 @ 1x`.
**Question:** limina's window "cuts off at the notch" in fullscreen on the internal display. What
is actually cutting it off, and which lever changes it?

## Vehicle

`probe.swift` builds the *same* window limina builds — titled + closable + miniaturizable +
resizable, `.fullScreenPrimary`, a layer-hosting content view with
`layerContentsRedrawPolicy = .never` — then toggles native fullscreen and dumps the geometry.
`build.sh` wraps it in a minimal `.app`.

Two false starts worth remembering, both of which look like AppKit bugs and are not:

- **A non-bundled binary cannot enter Spaces fullscreen.** `toggleFullScreen:` is a silent no-op:
  the window stays windowed and `styleMask.contains(.fullScreen)` stays false, with no error
  anywhere. It only works from a real bundle (hence `build.sh`). Two runs were spent reading
  "fullscreen" dumps that were nothing of the sort.
- **`makeKeyAndOrderFront` lands the window on the *main* screen**, ignoring the `contentRect`
  origin. With the lid open the probe went fullscreen on the external display and reported a
  notchless result. Re-seat with `setFrame` *after* ordering front.

## Measurements (built-in display)

| state | `screen.frame` | `window.frame` / `contentView.frame` | `screen.safeAreaInsets.top` | `auxiliaryTopLeftArea` |
|---|---|---|---|---|
| windowed | 1512x982 | as requested | 32 | `(-1512, 529, 663, 32)` |
| fullscreen, key absent or `false` | 1512x982 | **1512x949** | 32 | non-nil |
| fullscreen, key `true` | 1512x982 | **1512x982** | **0** | **nil** |

("key" = `NSPrefersDisplaySafeAreaCompatibilityMode` in the bundle's `Info.plist`.)

### What this settles

1. **The cut is AppKit insetting the fullscreen window below the camera housing**, not anything
   limina does. The default fullscreen window is 33 pt shorter than the panel and that strip is
   simply unusable.
2. **The plist key reads backwards.** "SafeAreaCompatibilityMode = true" is what hands the
   fullscreen window the **whole** panel; absent/false is what confines it. Do not reason about
   this key from its name — this table is the source of truth.
3. **`setFrame(screen.frame)` while fullscreen is ignored.** AppKit re-imposes 949. Not a lever.
4. **The `NSWindowDelegate` hook is consulted and then ignored.**
   `window(_:willUseFullScreenContentSize:)` fires (proposed `1512x949`), we returned `1512x982`,
   and the window still came up 949 tall. Not a lever either.
5. **Under the key, the notch height is only readable while NOT fullscreen.** Fullscreen zeroes
   `safeAreaInsets` and empties `auxiliaryTopLeftArea` — exactly when a policy needs the number.
   Any consumer must cache it per display from a non-fullscreen read.

## What limina does with this

`scripts/build-app.sh` now ships the key `true`, so AppKit always gives us the full panel, and the
policy becomes ours to make per VM rather than per build:

- `[display] notch = "avoid"` (default, and what limina looked like before): `fit::usable_content`
  re-insets the guest below the housing, leaving that strip black.
- `[display] notch = "extend"`: the guest gets all 982 pt and the housing overlaps its top edge.

`hostdisplay::notch_inset` implements the caching in (5).

This also closed a latent bug: host mode drove the guest to `screen.frame` (982 pt) while the
fullscreen content view was 949 pt, so `aspect_fit` letterboxed the guest on **all four sides**.
The guest resolution is now derived from the same usable area the fit uses.

## Caveat for dev runs

The key lives in the **app bundle**, so a bare `target/debug/limina` (what
`spikes/venus-draw-probe/boot-enhanced-efi-kk.sh` and `cargo xtask run` launch) still gets the
949 pt fullscreen window. Under `avoid` that happens to look identical; under `extend` the guest
is driven to 982 into a 949 view, so it letterboxes. Validate `extend` from a built
`Limina.app`, not from a dev run.

## Reproducing

```sh
./build.sh                          # or: SAFE_AREA_COMPAT=true ./build.sh
./NotchProbe.app/Contents/MacOS/NotchProbe
```

Needs the lid open (the internal panel must be an attached screen — the probe picks the first
screen with a non-zero top safe-area inset).
