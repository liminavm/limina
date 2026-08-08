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

---

# Round 6 — the Space-switch artifact: four fixes, none shippable

**Date:** 2026-08-08. Reported from dogfood: with `notch = extend` fullscreen, switching to another
Space and back animates the slide-in with the guest *inset below the housing*, then snaps up to
full-panel when the animation ends. Leaving looks correct. Returning before the switch resolves
keeps the strip.

**Instrumented rather than guessed:** `LIMINA_OVERLAY_TRACE=1` gained an `[OVERLAY-GATE]` line that
reports the gate inputs whether or not the overlay exists (the pre-existing trace could only
describe an overlay that was already up, i.e. it was blind to "why is it still down"), timestamped
in ms so signals can be timed against the ~0.4 s animation. `space-switch-probe.sh` drives the
sequence — **except that macOS ignores synthetic Ctrl-arrow for Space switching**: `osascript`
returns 0 and the WindowServer does nothing, so the switches themselves need a human.

## What the trace showed

`isOnActiveSpace` goes **false at the start of leaving** and **true only when the incoming
animation completes**. Both halves of the report follow: the outgoing animation shows a snapshot
taken while the overlay was still up (correct-looking), and the incoming one runs entirely in the
window where the gate is still false. The snap is the overlay being *rebuilt* — `show()`
re-parents the guest's whole view from the carrier into the overlay in one frame.

## Four attempts, each one moving the artifact rather than removing it

| attempt | result (human oracle, real notched panel) |
|---|---|
| 1. Overlay survives the switch (drop the `isOnActiveSpace` gate) | Geometry snap gone — guest animates full-panel both ways. **New:** black band beside the notch during the animation. |
| 2. Don't yield the *level* while our Space is away | Band gone. **New:** whole-screen black flash for several frames after the switch resolves; black rectangle over the wallpaper in Mission Control. |
| 3. Yield by hiding the overlay instead of dropping it to level 0 | Flash gone. **New:** guest renders inset, then snaps back — the original bug in a third costume. |
| 4. Make the yield clock measure the whole condition | Snap gone. **Black flash and Mission Control both return.** |

Attempt 4 + re-handing Core Animation the current frame on the way back (an idle guest presents
nothing, so nothing repairs the window on its own): **neither symptom improved**, which falsifies
"the layer simply stops being composited while off-screen".

## Shipped state: reverted, with two real bugs fixed on the way through

The `isOnActiveSpace` gate is **back**, so the reported snap is back. That is deliberate: the snap
is cosmetic, while the persistent overlay costs a whole-screen flash and a Mission Control
regression the user confirmed does *not* happen with the VM shut down.

Two genuine defects found while circling it, both kept:

- **Yielding dropped the overlay to level 0**, which puts it *behind* the carrier — whose content
  view is the empty placeholder `show()` leaves behind. So a dialog taking focus on our screen did
  not shrink the guest below the housing, it blanked the screen. Yielding now hides the overlay,
  which returns the view to the carrier and looks exactly like `notch = avoid`.
- **The yield clock measured the wrong condition** ("inactive and focus is here"), so it ran for
  the entire time the user was on another Space and was already expired on return — firing a yield
  in the frame after the animation. It now measures the full condition, including
  `on_active_space`, so returning restarts it.

## The redesign that would actually fix it

Stop moving the guest's view. Round 4 above already proved an **80 pt strip window** works: keep
the guest in the carrier permanently and float a strip-only overlay over the housing. A Space
switch then re-parents nothing, so there is no frame in which the geometry can differ — and
whatever Mission Control does with an above-menu-bar window costs a 33 pt strip rather than the
whole screen. The blocker round 4 recorded was *input*, and it has since been removed: the
uncaptured pointer path already maps through global coordinates
(`input::cg_global_to_view_point`, added for edge resistance), so the strip need not be dead to
clicks. Not attempted here — it is a design change, not a tweak.

# Round 7 — the strip-only redesign, and why hiding it had to stop using `orderOut`

**Date:** 2026-08-08, same session. The redesign above, built and driven on the real notched panel.

## The shape

The guest's `NSView` **never moves again**. It lives in the carrier for the process's lifetime. The
overlay becomes a second window covering only the housing band, with a *second* `CALayer` that is
handed the **same IOSurface** every present. Both layers get identical geometry in panel space
(`fit::notch_strip_frames`) and each window clips to its own bounds, so the seam is exact by
construction rather than by tuning — the strip's layer is just the carrier's layer frame shifted
down by the carrier's height.

Three consequences fall out, and each one killed a symptom from round 6:

- **A Space switch reflows nothing.** There is no re-parent, so there is no frame where the guest's
  geometry differs. The snap, the band-during-animation and the inset-then-snap costumes all go.
- **The guest's size is keyed on the *policy*, not on whether the strip is on screen**
  (`ExtendOverlay::claims_band`). The strip still hides while our Space is away — it is
  `fullScreenAuxiliary` but still drew over the neighbouring Spaces once their switch resolved,
  measured — and if the guest's height followed it down, every Space switch would put a full
  rescale back in. It doesn't.
- **The measurement path stops needing a special case.** The carrier's view is the inset one under
  both policies now, so `fullscreen_inset_measurement` no longer has to be skipped while the
  overlay is up.

Human oracle after the rebuild: *"seam invisible, notch area used"*, neighbours clean, no rescale.
Mission Control's black wallpaper background persisted and was **attributed to macOS, not us** by
the user (seen outside limina) — the round-6 version of it that *did* disappear with the VM shut
down is gone with the re-parent.

## The residual: the band flashing on the *external* display

One artifact survived: on some Space switches the 33 pt band appeared for a frame or two on the
BenQ — while limina was fullscreen on the **built-in**.

Two plausible causes were fixed first, on reasoning rather than measurement: `orderOut`'s
re-insertion re-binding the window's Space (round 1 measured that going to the *main* screen
regardless of frame), and `place()` caching rects computed from mid-switch `carrier.screen()`
readings. Both were real defects and both are kept. **Neither was the flash.** The flash survived
them, which is what forced the instrument.

`LIMINA_OVERLAY_TRACE` gained a `[STRIP]` line — the strip's `NSWindow::frame` at every
show/hide/place, stamped in epoch milliseconds so it interleaves with the detector's log. Two
consecutive lines, 20 ms apart, ended it:

```
[STRIP] …826059 show  cocoa=(-1512,528,1512,33) want=(-1512,528,1512,33) alpha=0.0
[STRIP] …826079 place cocoa=(  728,528,1512,33) want=(-1512,528,1512,33) alpha=1.0
```

**The window server parks a hidden `fullScreenAuxiliary` window while its Space is away, and does
not tell AppKit.** At `show()` the frame read back as the correct `-1512` — so the "is it already
in the right place?" test passed and the write was skipped — while the server had the window at
x=728, i.e. on the BenQ. Alpha went to 1 at the parked position; the next tick's `place()` found
728, moved it home, and that round trip *is* the flash. `NSWindow::frame` is our last write, not
the server's copy of it, and there is no API that reports the difference.

The fix is one line in the wrong place, twice over:

- Forcing the write from `show()` **does not work.** Two `setFrame:display:` calls in one pass
  coalesce against the frame the pass *started* with, so nudging away and back leaves the nudge
  and drops the destination — measured, and visible in the detector as the band revealing 1 pt
  low. You cannot land on a rectangle in a pass that began at that rectangle.
- So **`hide()` desynchronises deliberately**: it parks AppKit's cached frame one point *down*
  while alpha is 0 and nothing can see it. `show()`'s ordinary "is the frame right?" test is then
  false, the write happens, and it lands in the same transaction as the alpha. One point down
  rather than up because if the write is a frame late the band sits 1 px low, leaving a 1 px gap
  at the very top — which is the camera housing, black either way.

Also: the strip is **born at the band**, not at the screen frame. An opaque black window the size
of the panel, even for the single frame before `place()` corrects it, is a full-screen flash.

**Verified** over ~10 human-driven Space switches (including animation-interrupting ones) and a
fullscreen exit/re-enter: the human oracle reports no flash, and in the detector's log the x=728
population is gone — every remaining off-display sample has the carrier at the same x, i.e. the
slide.

The route to that sentence is itself the cautionary tale, and the detector section below has the
detail: the discriminator was first computed by a post-hoc pass, then built into the tool as
`SOLO` — whose *first live run printed 26 hits on a clean build*, all false. The rule was wrong,
not the code. With the corrected rule (companion-x rather than "somebody else is home") the tool
prints its own verdict: `7761 changes, 1448 off-display (0 SOLO)`.

## `flash-detector.swift` — making "no flash" a measurement

A one-or-two-frame artifact on an intermittent, human-driven event is not something eyeballing can
clear. `flash-detector.swift` polls `CGWindowListCopyWindowInfo` (public API, no Screen Recording
permission needed for bounds or alpha) every 5 ms and logs each *change* in a limina window's
bounds, alpha or on-screen state — a change log, because the question was never "where was it" but
"did our alpha lead or trail the move", which is what separates "we revealed it in the wrong place"
from "the server moved it behind our back":

```
swiftc -O spikes/notch-fullscreen/flash-detector.swift -o /tmp/flash-detector
/tmp/flash-detector             # displays read from CoreGraphics; Ctrl-C for the summary
```

Two things it got wrong at first, both worth keeping in mind:

- **The first version took the expected display origin on the command line** and was given the
  `NSScreen` value. `CGWindowBounds` is in CG's top-left-origin global space and `NSScreen.frame`
  is not, so every reading was misclassified. It now asks `CGDisplayBounds` itself.
- **Off-display is not the same as wrong.** A Space slide legitimately carries every window of that
  Space across the boundary, and the first run flagged 704 samples that were all just the animation
  — the carrier included. The signal is the strip off its display **while the rest of the app is
  still home**, which the summary counts as `SOLO`. That is the number that has to be zero, and a
  post-hoc Python pass to compute it is now built in.

- **Off-display is not even the right *unit*.** The first `SOLO` rule was "off its display while
  something else of ours is home", and it fired 26 times on a clean build. At every one the strip
  and the carrier sat at the *identical* x, mid-slide — they only disagreed because the 33 pt strip
  fits entirely inside the external display's y range while the 949 pt carrier is clipped by that
  display's bottom edge, tipping the area majority for one and not the other. What actually
  distinguished the real bug was **disagreement in position**: the strip at x=728 while the carrier
  was at x=-1512. `SOLO` now means "off-display and no other window of ours within 2 pt of the same
  x". Replayed over the recorded logs: 0 on the fixed build, and all 13 of the original x=728
  samples still caught.

Synthetic Space switches remain impossible (round 6), so the human drives and this watches.

## The hotplug sequel: a cache of intent is not a cache of reality

Found by the user while driving the above: after unplugging and replugging the external monitor,
the guest rendered **entirely below the housing** while the band still showed content — two GNOME
panels — healing on anything that forced a re-measure (leaving fullscreen, or the chrome ask).

**This section is also a record of two wrong diagnoses, both reached by deduction from a partial
reading, and both cheap to have avoided.**

*Wrong #1.* The `[STRIP]` trace showed `hide cocoa=Some((0.0, 948.0, 1512.0, 33.0))` — a housing
strip at x=0, which I called a housing strip on the *external* display. It is not: with the
external monitor unplugged the built-in **becomes** the display at origin 0. The frame was correct.

*Wrong #2.* That misreading suggested the notch caches were confusing the two panels, because they
were keyed on `CGDirectDisplayID` and macOS renumbers those across an arrangement change. That is a
real latent hazard and it is now fixed — the caches key on `hostdisplay::panel_key` (vendor / model
/ serial from the panel's own EDID), with `the_panel_key_survives_a_hotplug_renumbering_the_display_ids`
as the regression, confirmed RED against the old behaviour. **But it was not this bug**: the trace
shows the built-in kept `id=1` across the replug, so no renumbering ever occurred.

What ended it was refusing to reason any further. A `[GEOM]` line logging every input to the
guest's height, read *while the user held the broken state*, said every number was correct and
unchanged — one `[GEOM]` line for the whole session, `claims_band=true`, `strip_inset=33`,
`guest_area=1512x982`, and the guest itself at 3024x1960. A screenshot then showed the top bar
drawn **twice, 33 pt apart**. So the two layers disagreed about where the image started while every
input agreed. A `[LAYER]` line — what Core Animation *actually holds*, rather than what we last
asked for — named it on the next repro:

```
[LAYER] … carrier=(0,0 1512x949) strip=(0,-948 1512x980) target=FitRect{0,1,1512,980}
```

`target` correct. Strip correct. **The carrier's layer reset to its view's bounds.** AppKit resets
a layer-hosting view's layer frame to the view bounds on a layout pass — a display reconfiguration
is one — and says nothing. The strip recovered because `place()` rewrites its layer every tick; the
carrier's write was guarded by `target != fit_cell`, **a cache of what we asked for**, and the
intent had not changed, so the drift was permanent. The guest was squashed into the 949 pt view
(bar 2, at the carrier's top) while the strip went on showing the top of the real 980 pt fit
(bar 1). It healed on exactly the actions that *change* the intent — a fullscreen toggle, the
chrome ask — which is why it looked so arbitrary.

`layer_frame_differs` compares against the layer now, not against our cache. Verified by the user
and in the trace: through a full replug the layers stay `strip.y == carrier.y - carrier_height`
with matching heights, and the drift line never appears.

A second writer found on the way and fixed with it: the present path's modeset refit fitted the
guest into the **raw content view**, which under `extend` is exactly the housing inset too short,
and wrote that to both the carrier's layer and `fit_cell`. `strip_inset_now` is the one definition
both callers use now — the same "one number" rule `fit::panel_size` already stated and this path
quietly broke.

## The pointer that vanished in the band

Reported the same day, in the same fullscreen `extend` session: the pointer disappears on entering
the housing band — but the guest keeps reacting to it (the GNOME top bar highlights, clicks land).

Four things had to be established before the mechanism was even a candidate, and three of them
killed a theory I would otherwise have implemented:

1. **The chrome ask, suspected first, was not involved.** The run's own `[OVERLAY-GATE]` line shows
   a clean grant/withdraw cycle. Separately: the *original* complaint that day — "the band stays put
   on the chrome ask" — did not reproduce on the fixed build. That is consistent with the layer-drift
   fault above (the ask was one of the things that *healed* the two-panel state, so a run in the
   drifted state is exactly where it would misbehave), but it is not separately proven.
2. **The guest holds a hardware cursor plane.** `sudo cat /sys/kernel/debug/dri/0/state` in the guest
   shows `plane-1`, `AR24`, allocated by the KMS thread. So the guest never draws the pointer into
   the scanout, and the "the strip is showing a stale picture" family of theories was dead.
3. **A warp is not a measurement.** The first probe placed the cursor with
   `CGWarpMouseCursorPosition`, which posts **no events** — `on_motion` never runs, so the trace is
   silent whatever the truth is. Silence that looks like a finding is worse than no probe. Real
   synthetic `mouseMoved` events, and a *control* sweep in the middle of the guest first, so band
   silence can only mean one thing.
4. **The probe could not drive the pointer at all** — and that was the answer. `[GRAB] grabbing`:
   in fullscreen the grab enters pointer **capture**, which warps the cursor back every event.

Capture is the whole mechanism. It hides the host `NSCursor` and composites the guest's cursor into
a `CALayer` — a sublayer of the **carrier's** scanout layer. That layer is a housing inset taller
than the carrier's window, so its top band is clipped by the window and drawn by the strip instead,
and the strip had a copy of the picture but not of the cursor. Hence: vanishes exactly in the band,
input unaffected. `ExtendOverlay::strip_cursor_layer` hangs the copy off the strip's scanout layer,
where the same computed frame is correct for both and each window clips its own share.

The generalisation is the one the strip design owes from here on: **a second window showing part of
the same picture must mirror every layer the first one composites, not just the picture.** The
scanout got a copy on day one because it was obvious; the cursor did not, because it only exists in
a mode nobody was in while building it.

### The lesson, which is not about AppKit

Three times in this session the answer came from measuring the thing itself rather than the thing
we believed about it: `NSWindow::frame` is our last write, not the server's; `fit_cell` is our last
intent, not the layer's; a strip "off its display" is not the same as a strip drawn *alone* off its
display. Every one of those is a cache of an intention being read as an observation. When state can
be changed by something other than us — the window server, AppKit's layout, the compositor — the
only safe comparison is against the state.
