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

> **RESOLVED — see "Round 2" below. The table is accurate and its conclusion (2) is wrong:
> `contentView.frame` is not what gets drawn.** Keep reading before acting on anything here.

> **⚠️ DISPUTED, 2026-08-01 (same day).** Dogfood on **dogfood-mac** (14" M4 Pro, panel at "More
> Space" — `screen.frame` 2048x1330) contradicts row 3 of this table. With the key shipped
> `true`, `notch = "extend"`, and the guest correctly driven to the full 2048x1330, the guest
> still does **not** reach under the camera housing — the housing does not overlap the GNOME
> panel clock, and the content is inset. Apple's own documentation reads the same way as the key's
> name and the opposite of row 3: compatibility mode "changes the active area of the display to
> avoid the camera housing".
>
> A methodological hole that could explain it: every arm below was built to the **same path with
> the same bundle identifier**. macOS keeps notch policy *per app* (that is what the Finder
> "Scale to fit below built-in camera" checkbox writes) and LaunchServices caches a bundle id's
> registration, so the arms may not have been independent. `build.sh` now takes `APP_NAME` /
> `BUNDLE_ID`, and `run-remote.sh` builds three separately-identified arms (absent / false /
> true) on another Mac. **Do not act on row 3 until that re-measurement lands.**

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

---

# Round 2 — measured on dogfood-mac, and with the *pixels* this time

**Date:** 2026-08-01, later the same day.
**Host:** dogfood-mac, 14" M4 Pro, macOS 26.5.1. Built-in panel at "More Space":
`screen.frame = 2048x1330`, `safeAreaInsets.top = 43`, external DELL 3840x2160 as main.
**Trigger:** shipping the round-1 conclusion produced a fullscreen guest that still does not
reach under the camera housing — the housing does not overlap the GNOME panel clock.

## The arms actually were independent this time

`build.sh` now takes `APP_NAME`/`BUNDLE_ID` and `run-remote.sh` builds each arm as its own app,
because macOS keeps notch policy per app. Round 1's shared bundle id was a real methodological
hole — it just wasn't the one that mattered.

| arm | fullscreen `contentView` | `screen.safeAreaInsets.top` (fullscreen) | `auxiliaryTopLeftArea` |
|---|---|---|---|
| key absent | 2048x**1286** | 43 | non-nil |
| key `false` | 2048x**1286** | 43 | non-nil |
| key `true`  | 2048x**1330** | **0** | **nil** |

Round 1 reproduces exactly. `NSPrefersDisplaySafeAreaCompatibilityMode = true` really does hand
the fullscreen window a frame covering the whole panel, and really does zero the insets.

## ...and it means nothing, because the strip is masked anyway

The human oracle: with the `true` arm fullscreen and its content filled green, **the strip beside
the camera housing was black**. Not green. The window's frame says it owns those 44 points; the
screen says otherwise.

So compatibility mode is worse than the name suggests. It does not grant the housing area — it
grants a *frame* that includes it, zeroes `safeAreaInsets` so nothing can tell, and lets the
system mask the strip regardless. The honest arms (absent/`false`) at least report 1286 and a
43 pt inset, so a caller can see what it is getting.

**Round 1's conclusion (2) — "the key hands the fullscreen window the whole panel" — was drawn
from `contentView.frame` alone and is wrong.** A frame is a proxy. The repo already knows this
(`CLAUDE.md`, "Pixel-verify; proxies lie"); it cost a day anyway. The lesson is narrower and
sharper than the existing one: *AppKit geometry can disagree with the compositor*, so a geometry
dump is not evidence about pixels even when the geometry is what you are asking about.

## What can reach the strip: a borderless window, not a Space

Fourth arm (`NOTCH_LEGACY=1`): skip `toggleFullScreen:` entirely, take a `.borderless` window,
`setFrame(screen.frame)`, `NSApp.presentationOptions = [.hideMenuBar, .hideDock]`, raise the
window level. Both arms paint a pink band across their top 80 pt so the eye has something
unambiguous to look for.

- **native Spaces fullscreen (key `true`): band black beside the housing.**
- **borderless at `screen.frame`: band PINK beside the housing.**

That is the whole answer. Native fullscreen can never draw beside the camera housing on this
macOS; a borderless full-panel window can.

## Consequences for limina

1. **The shipped `NSPrefersDisplaySafeAreaCompatibilityMode = true` is actively harmful** and
   should come out. It buys nothing (the strip is masked either way) and costs the truth: with
   it, `screen.safeAreaInsets` reads 0 in fullscreen — which is why `hostdisplay::notch_inset`
   needs a per-display cache at all — and the fullscreen `contentView` is 44 pt taller than what
   is visible, so a guest sized to it has its top rows hidden under the mask.
2. **Without the key, `avoid` needs no work from us**: AppKit already insets the fullscreen
   window to 1286, and `safeAreaInsets` stays readable *while fullscreen*, so the cache can go.
3. **`extend` cannot be done with native fullscreen at all.** Delivering it means a borderless
   full-panel window — which is not a Space: no Mission Control slot, no fullscreen animation,
   different Cmd-Tab and green-button behaviour, and `styleMask.contains(.fullScreen)` goes
   false, which the edge-resistance gate currently keys on.

## Reproducing round 2

```sh
./run-remote.sh user@dogfood-mac          # builds absent/false/true arms, each its own bundle id
# on that Mac, lid open:
/tmp/notch-probe/NotchTrue.app/Contents/MacOS/NotchTrue
NOTCH_LEGACY=1 /tmp/notch-probe/NotchLegacy.app/Contents/MacOS/NotchLegacy
```

Watch the pink band, not the numbers.

---

# Round 3 — what Apple actually documents (and it agrees)

**Date:** 2026-08-01, later still. Prompted by a fair challenge: was there an opt-in that makes
*native* fullscreen use the strip, registered in the plist and in code? Apple's own pages answer
it, and they line up with round 2 rather than against it. (`developer.apple.com` is
JS-rendered — `WebFetch` returns only the page title. Read it in a real browser.)

**`NSScreen.safeAreaInsets`** — the decisive one:

> If your app offers a **custom full-screen experience**, apply the specified insets to the
> screen's frame rectangle to obtain the area within which it is safe to display your content.
> […] If your app uses the system's full-screen experience, you don't need to account for the
> safe area in your window. When you call your window's `toggleFullScreen(_:)` method to enter
> full-screen mode, **the system automatically positions the window's contents within the safe
> area.**

Unconditional, with no opt-out offered. The alternative Apple names — "a custom full-screen
experience" — is precisely the borderless full-panel window round 2 arrived at empirically.

**HIG, Going full screen › macOS:**

> Use the system-provided full-screen experience. […] some Mac models include a camera housing
> that occupies an area at the top-center of the screen. Using the system's full-screen support
> **automatically accommodates** this area.

**`NSPrefersDisplaySafeAreaCompatibilityMode`** — read properly, it is not a fullscreen key at
all. Compatibility mode is the system "changing the active area of the display to avoid the
camera housing", and:

> The system activates this compatibility mode when an app that requires it **places a window
> behind the camera housing** in the current desktop or full-screen space.
> […] Set the value of the key to true to always run your app in compatibility mode, and set it
> to **false to never** run your app in compatibility mode.
> […] If your app's `Info.plist` file includes the key, the Finder doesn't add that checkbox.

So the round-2 measurement of `true` was not AppKit being perverse — placing our window behind
the housing is the documented *trigger*, and compatibility mode then blacks the strip. The frame
still reading 1330 with zeroed insets is the part that misleads, and is why the geometry dump
lied.

## The one thing this changes

Round 2 concluded "drop the key". That is half right: the key must be **present and `false`**,
not absent. Panel fullscreen places a window behind the camera housing — the documented trigger —
so leaving the decision to a Finder Get Info checkbox we do not control risks the system masking
the strip out from under `extend`. `false` means never, and removes the checkbox.

`scripts/build-app.sh` ships `<false/>`.

## Status

Native fullscreen cannot use the housing strip: measured four ways on two Macs, and documented by
Apple twice. `notch = "avoid"` uses the system experience and lets it do the inset; `extend` is
the custom full-screen experience Apple's own wording contemplates.

---

# Round 4 — you can have the Space AND the strip

**Date:** 2026-08-01, last round. Prompted by: "wouldn't we be able to reserve a space somehow?"

## Private Space APIs are a dead end

`CGSSpaceCreate` / `CGSAddWindowsToSpaces` / `CGSShowSpaces` exist in the private CGS/SkyLight
surface, but an ordinary app cannot use them to make a real Mission Control desktop. yabai's
maintainer, on why yabai needs SIP off:

> The function itself is not protected, but the target of the function (e.g the window to modify)
> is protected, so the function results in a no-op, because you are not authorized to modify that
> window. […] The Dock.app connection to the WindowServer is flagged as a universal owner for all
> windows and is allowed to pass that access restriction.

So a genuine Space needs SIP disabled plus code injected into Dock.app. Off the table.

## But an auxiliary window paints where the fullscreen window cannot

Arm `NOTCH_AUX=1`: enter **native** fullscreen normally, then float a borderless window with
`collectionBehavior = [.fullScreenAuxiliary]` over the camera-housing strip.

**Result (dev-mac, human oracle): pink beside the housing.**

Public API only. The mask that stops a fullscreen window using the strip does not stop an
auxiliary window floating over the same Space from doing it.

So the two are separable after all:

| | Space / Mission Control / swipe | housing strip |
|---|---|---|
| native fullscreen alone | yes | no |
| borderless full-panel (round 2) | **no** | yes |
| native fullscreen + aux strip window | **yes** | **yes** |

## What building on it would take

The guest is driven to the full panel height. The main fullscreen window shows the bottom
`frame.height - inset` rows and the strip window shows the top `inset`. The clean way to split it
is **not** `contentsRect` unit-coordinate arithmetic (easy to get flipped by one strip) but giving
both windows a layer at the *same* geometry in screen space and letting each window clip: the
strip window's layer frame is simply offset by `-(frame.height - inset)`.

The real risk is **input, not pixels**. An `ignoresMouseEvents` strip leaves the guest's top rows
visible but dead — and on a GNOME guest those rows are the top bar, so the clock and menus would
be visible and unclickable, which is worse than not having them. The uncaptured pointer path
would have to stop deriving position from the main view's `locationInWindow` and use the event's
global location instead (`input::cg_global_to_view_point`, already added for edge resistance),
so the pointer maps to guest pixels regardless of which of the two windows it is over.

---

# Round 5 — carrier + overlay: the design that gives up nothing

**Date:** 2026-08-01. The user's idea, and it beats both of mine: use native fullscreen as a
*carrier* for the Space, and float the borderless full-panel window **on top of it** as an
auxiliary window, rather than choosing between them.

Arm `NOTCH_AUX=full`: native `toggleFullScreen:`, then a borderless `KeyableWindow` with
`collectionBehavior = [.fullScreenAuxiliary]` at `screen.frame`, level `.mainMenu + 1`.

Measured (dev-mac, human oracle for the pixels, printed for the rest):

- **Pink reaches beside the camera housing.** A full-panel overlay is not masked, same as the
  80 pt strip.
- **The menu bar does not appear over it**, even shoving the pointer at the top edge. The overlay
  is above menu-bar level, so the reveal happens *behind* it.
- **`canBecomeKey=true isKey=true`** — it takes keyboard focus (it is a `KeyableWindow`).

| | Space / Mission Control | housing strip | chrome at the top edge |
|---|---|---|---|
| native fullscreen alone | yes | no | reveals over the guest |
| borderless full-panel (round 2) | **no** | yes | cannot reveal |
| native carrier + full-panel overlay | **yes** | **yes** | **cannot reveal** |

## What this retires

**Top-edge resistance becomes unnecessary** — and so does the `KEEPOUT` band that exists only
because macOS reveals the menu bar on contact. Nothing can appear over the overlay, so the cursor
may sit on the top row harmlessly, which also means the guest's own top bar and top-left hot
corner get the pointer at their true edge instead of two points short.

Resistance stays worth keeping **at the sides**, where the pointer can still slip to another
display while aiming for a guest edge. That is the half of `EdgeResist` the user asked to keep.

## Aside: macOS has pointer *lock*, not pointer *confine*

Wayland has both `locked_pointer` (freeze, keep deltas) and `confined_pointer` (keep inside a
region, still moving). macOS has only the first: `CGAssociateMouseAndMouseCursorPosition(false)`,
which capture mode already uses. There is no `ClipCursor`/`XGrabPointer` equivalent, so confining
to a rect has to be emulated with warp-clamping — which is what `fit::capture_step` and
`fit::EdgeResist` do. The resistance code is the standard workaround, not a reinvention.
