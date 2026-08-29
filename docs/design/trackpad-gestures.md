# Trackpad gestures: a guest-side multitouch device with strict contact ownership

Status: DESIGN. MT device not implemented. The companion quick win SHIPPED 2026-07-28:
hi-res scroll (f1a8e56) — see §Independent quick win. A raw-multitouch alternative that
would reopen the ownership rule under capture is under evaluation — see §Alternative: raw
multitouch capture.

## Why

The guest never sees the trackpad as a trackpad. Today's pointer stack
(`crates/limina-input/src/backends.rs`) exposes an absolute tablet (`ABS_X`/`ABS_Y`,
`INPUT_PROP_POINTER`) for seamless mode plus a separate relative mouse for pointer
capture; macOS's gesture recognizer collapses every trackpad gesture into synthesized
events before we forward anything. Two-finger swipes arrive as `ScrollWheel` and
`emit_scroll` (`crates/limina/src/window/input.rs`) quantizes the pixel deltas to ±1
`REL_WHEEL` clicks — jerky scroll, no kinetic feel in the guest, and pinch/multi-finger
gestures are lost entirely (evdev has no "pinch event"; libinput computes gestures from
raw MT contacts, so without an MT device they are unforwardable in principle).

The fix is a third virtio-input device: a multitouch touchpad the guest's libinput
classifies as a real clickpad, fed from AppKit's per-finger indirect touches. The guest
then does its own gesture processing — pixel-precise two-finger scroll, kinetic in GTK,
real pinch, tap-and-drag under capture — with **zero guest-side deliverables** (stock
virtio_input + libinput; pure additive, two-tier clean).

## The ownership rule (the load-bearing decision)

macOS's WindowServer recognizes 3+-finger gestures (Mission Control, Spaces, Launchpad)
at the system level and **cannot be suppressed per-app** — the app still receives the
touches, but the host acts too. Any forwarding of those contacts guarantees
double-interpretation. So contact groups are partitioned crisply, and the partition is
the design:

| physical contacts | owner | delivered to guest as |
|---|---|---|
| 1 finger | **host** (tablet, host pointer ballistics) | `ABS_X/Y` tablet motion, as today |
| 2 fingers | **guest** (scroll + pinch) | real MT contact pair on the new device |
| 2 fingers + Fn held | **guest** | fully **synthetic 3-finger** group (see below) |
| 3+ physical fingers | **host, forever** | nothing — never forwarded |

**This premise is reopened for the captured case** — see §Alternative: raw multitouch
capture. It stands unchanged for seamless mode.

Single-finger sequences are never forwarded, so the MT device never drives the guest
cursor and cannot fight the tablet for it — that restriction is what makes a
guest-side touchpad compatible with the seamless host-cursor=guest-cursor model. Full MT
forwarding (1-finger motion included) is a possible *hard-capture* refinement later; it
is explicitly out of scope for round one, and the 3+-finger ignore rule stays global
(capture included) because host gesture recognition fires regardless of our capture
state.

## Host side: touch source and device config

- **Source:** AppKit indirect touches — `NSView.allowedTouchTypes = .indirect`, then
  `touchesBegan/Moved/Ended`. Per finger: normalized `[0,1]` position, stable identity,
  phase, plus `deviceSize` in points. Not raw HID, but exactly the shape a Linux MT
  touchpad reports.
- **Device config (round one):** `EV_ABS` with `ABS_MT_SLOT` (3 slots),
  `ABS_MT_TRACKING_ID`, `ABS_MT_POSITION_X/Y` + legacy `ABS_X/Y`; `EV_KEY` with
  `BTN_TOUCH`, `BTN_TOOL_FINGER`, `BTN_TOOL_DOUBLETAP`, `BTN_TOOL_TRIPLETAP`,
  `BTN_LEFT`; `INPUT_PROP_POINTER` + `INPUT_PROP_BUTTONPAD`. **`abs_info.res`
  (units/mm) is mandatory** — libinput refuses/degrades touchpads without resolution;
  derive it from `deviceSize`. The libkrun vtable already carries `res`
  (`third_party/libkrun/include/libkrun_input.h:103-109`), so this may need no libkrun
  change beyond the new config backend. No `QUADTAP`, no 4th slot until the 4-finger
  synth ships — advertise only what can actually arrive.
- **Gating:** forwarding gates on **cursor over the VM view** (like scroll routing), not
  on key-window status — orthogonal to the soft *keyboard* grab, which stays
  key-gated. Applies in soft/seamless mode and capture alike.

## Fn chord: synthetic 3-finger gestures

3+-finger gestures are recovered without ever violating the ownership rule: the physical
count stays at 2 (WindowServer sees nothing it wants), and the guest receives a fully
synthetic contact group.

- **Chord = Fn (Globe).** Bare Shift/Ctrl are disqualified: Shift+scroll (horizontal
  scroll) and Ctrl+scroll (zoom) are established guest semantics that must not morph
  into workspace swipes. Fn has no guest scroll meaning, typically isn't forwarded as a
  guest modifier at all (no leakage into the guest key stream), and "Globe = input
  magic" is Apple's own trained mental model. Chord should end up in the customizable
  keybindings config eventually.
- **Sample at second-finger touchdown, lock per sequence** (momentum tail included).
  Mid-gesture modifier changes must not morph finger count — libinput treats count
  changes as gesture cancellation. Chord pressed mid-scroll takes effect next sequence.
  Fn-down with no second finger does nothing — a lone Fn tap keeps its host binding
  (emoji picker et al.) untouched.
- **Synthesize the whole group, don't augment.** Generate all 3 phantom contacts (neat
  row around the real centroid, driven by the measured average delta; scale contact
  spread by the real pinch ratio for pinch). Fully synthetic contacts are perfectly
  coherent for libinput's swipe detection; mixing real geometry with a fake finger
  invites edge-clipping and coherence edge cases. The real touches are just puppet
  strings.
- **Round-one scope: 3 fingers only.** GNOME ≥40 binds everything to 3 fingers
  (horizontal = workspace switch, vertical = overview/app grid); 4-finger is unbound by
  default (KDE Plasma uses it — deliberately not our problem). 4-finger later = second
  chord (e.g. Fn+Shift) + `QUADTAP` + one more slot; the synthesis path is
  finger-count-parameterized from the start.

## Dedupe and teardown (the state machine)

While a forwarded 2-finger sequence (plus its momentum tail) is in flight:

- **Swallow** host-synthesized `ScrollWheel`, magnify, and tap-generated clicks (e.g.
  host two-finger-tap right-click) — otherwise the guest gets the gesture twice. The
  capture tap tracks "MT sequence in flight" and swallows there.
- **Suppress `emit_scroll` entirely** for trackpad-sourced scrolls when the MT device is
  active for the sequence.

Teardown: on *any* transition — cursor leaves the view, window loses key, capture
toggles, a third physical finger lands (host is taking over), or one finger of the pair
lifts — release every guest slot cleanly (`tracking_id` −1, `BTN_TOUCH` up, SYN) so the
guest never sees stuck fingers. Same discipline as the soft-grab modifier flush
(`InputState::exit_soft_grab`).

## Forgiving drags (the tap-and-drag question)

libinput's tap-and-drag + drag-lock grace period is a **touchpad-class** feature; the
tablet is a generic pointer and can never engage it. The answer is layered by mode:

- **Seamless/soft:** the host owns pointer ballistics, so the host owns drag semantics —
  macOS Accessibility → Pointer Control → Trackpad Options → "Use trackpad for
  dragging" with **drag lock** keeps the virtual button down across finger lifts, and
  the guest sees one unbroken `BTN_LEFT` through the tablet. Works today, zero code;
  document it.
- **Hard capture (future full-MT):** guest libinput provides tap-and-drag natively.
- macOS *three-finger drag* is fine under the ownership rule: 3 physical fingers are
  never forwarded, and the host-synthesized button+motion flows through the tablet.

## Independent quick win: hi-res scroll (SHIPPED f1a8e56, 2026-07-28)

`emit_scroll` now feeds precise macOS deltas through per-axis v120 accumulators
(`ScrollAxis` in `crates/limina/src/window/input.rs`): `REL_WHEEL_HI_RES` /
`REL_HWHEEL_HI_RES` events for every input (53 pt of finger travel = one detent = 120
units, rounding carry preserved), plus legacy detent events on ±120 boundaries for
pre-hi-res guest stacks — libinput ignores those when hi-res is present, per its
wheel-API contract; the trap to never hit is advertising HI_RES without sending it
(libinput then drops wheel scroll entirely). Both pointer devices advertise the HI_RES
codes. Physical wheels (non-precise deltas) keep the legacy one-notch-per-event mapping
in both rates. Momentum-phase events flow through the same path, so guest kinetic decay
comes free. (The MT device supersedes this for trackpad scrolls but hi-res still serves
mice and any swallowed-path fallback.)

## Alternative: raw multitouch capture (UNVERIFIED)

macOS's private `MultitouchSupport.framework` publishes the trackpad's **raw contact
stream** — every finger's position, ellipse geometry, pressure and density, at sensor
rate — to any userland client that registers a callback. That is strictly richer than
AppKit `NSTouch`, and it is the source a guest-side MT touchpad actually wants.

The open question is whether reading it can also be made to **stop macOS from
recognizing gestures on the same contacts**. If it can, the ownership rule collapses to
"under capture, every contact belongs to the guest": no Fn chord, no synthetic phantom
fingers, real 3- and 4-finger swipes with real geometry, guest-native palm rejection and
tap-and-drag. If it cannot, the raw stream is still worth taking as a better *source*
under the ownership rule as written — that outcome is a landing point, not a failure.

### The cheap unlock: give macOS four fingers, take three

There is a path to 3-finger guest gestures that needs **no suppression, no private API and
no code**: macOS's Mission Control / Spaces / App Exposé swipes are individually
configurable to three *or* four fingers. Set them to four, and three-finger contacts are
no longer claimed by the host at all — which is exactly the count GNOME ≥40 binds
everything to. This is what BetterTouchTool asks its users to do, arrived at
independently as the pragmatic answer.

The relevant defaults, in `com.apple.AppleMultitouchTrackpad` (built-in) and
`com.apple.driver.AppleBluetoothMultitouch.trackpad` (external Magic Trackpad) — `2` is
enabled, `0` disabled:

    TrackpadThreeFingerHorizSwipeGesture   ← set 0 to free 3-finger horizontal
    TrackpadThreeFingerVertSwipeGesture    ← set 0 to free 3-finger vertical
    TrackpadFourFingerHorizSwipeGesture    ← leave 2; macOS keeps four
    TrackpadFourFingerVertSwipeGesture     ← leave 2
    TrackpadThreeFingerDrag                ← must be 0, or Accessibility claims 3 fingers
    TrackpadThreeFingerTapGesture          ← must be 0

Two things follow for the design. First, **limina should read these, not write them**:
they are global, persistent user settings, they belong to the user, and the settings
daemon does not reliably pick up a `defaults write` anyway. Detect the state and *tell*
the user which counts the host has claimed — a first-run/preferences hint, not a mutation
we own and could leak. Second, the forwarding path must be **configurable by finger
count** rather than hardcoding "2 is ours, 3+ is theirs", because which counts are free
is now a per-machine fact we can query.

This does not make the suppression question moot — it is opt-in per user, it cannot be
the out-of-the-box experience, and it buys nothing for 4-finger guest gestures. But it is
available today, it is how the reference implementation in this space actually ships, and
it should be the first thing tried.

### What is established

- **Reading raw contacts works, from an ordinary app, no kext and no daemon.**
  `MTDeviceCreateList` → `MTRegisterContactFrameCallback` → `MTDeviceStart`, all via
  `dlopen`/`dlsym` on
  `/System/Library/PrivateFrameworks/MultitouchSupport.framework/MultitouchSupport`.
  Three reference implementations, cloned under `~/Projects/`:
  **`OpenMultitouchSupport`** (Kyome22, MIT) is the one to read — maintained (May 2026,
  macOS 15+), and its `Framework/OpenMultitouchSupportXCF/OpenMTInternal.h` is the best
  available declaration of the private API: the `MTTouch` layout, the named `MTTouchState`
  enum, and the device-property calls. **`M5MultitouchSupport`** (MIT, 2015) is its
  ancestor, same data model, worth reading only for its listener/threading design.
  **`GutchinTouchTool`** is the Swift/`dlopen` reference — the binding *mechanics* to copy,
  though its data model is wrong (below). None of the three attempts suppression.
  Both ObjC libraries *link* the private framework at build time; we should keep
  `dlopen`/`dlsym`, which degrades gracefully when a symbol goes away.
- **Delivery is device-global, not window-scoped.** The callback fires for every touch
  on the machine regardless of focus, key window, or cursor location. Focus gating is
  entirely ours to impose — the same gate the capture tap already applies.
- **Reading does not suppress anything.** GutchinTouchTool's README claims the raw path
  "bypass[es] the system gesture recognizer entirely"; its code does not. Its swipes come
  from `NSEvent` `scrollWheel`/`magnify` global monitors — the *cooked* stream — and raw
  frames feed only taps, TipTaps, circles and the visualizer. Its sole suppression is a
  session `CGEventTap` eating mouse clicks and moves. The raw stream runs **in parallel**
  with system recognition; nothing in that project disables it.
- **No shipping app has demonstrated suppressing the 3-finger Mission Control swipe.**
  BetterTouchTool — the reference implementation of this whole technique — instructs users
  to uncheck the gesture in System Settings before binding it. That is the strongest
  available evidence that the plain userland levers do not reach WindowServer's
  recognizer, and it is why everything below is a hypothesis list rather than a plan.
- **The `MTTouch` layout is 96 bytes, and the popular Swift binding gets it wrong.** Both
  MIT libraries declare `normalizedPosition` as an `MTVector` — position *and* velocity,
  four floats — followed by `total` (capacitance) and `pressure`. That puts `majorAxis` at
  offset 60 and `density` at 92, for a 96-byte stride. GutchinTouchTool instead declares
  `normalizedPosition` as two bare floats, which shifts everything after offset 40 by 8:
  it reads "majorAxis" at 52, where the real field is `pressure`. Its runtime stride
  auto-detection (candidates 80…128) lands on the true 96 anyway, so position survives and
  the contact *geometry* it feeds its visualizer does not. **Take
  `OpenMultitouchSupport`'s header as the layout reference** — it is MIT, it is
  maintained against current macOS, and it agrees with the 2015 lineage.
  Better still where they suffice, prefer the framework's **opaque-handle accessors**
  (`MTRegisterPathCallbackWithRefcon` + `MTPath_getPosition`/`getForce`/`getVelocity`/
  `isTouching`/`wasRejected`, and `MTContact_getEllipse*`), which cannot be shifted by a
  layout change at all. A struct that has to be stride-detected at runtime is the argument
  for both.
- **The device answers the `abs_info.res` question directly.**
  `MTDeviceGetSensorSurfaceDimensions` returns the physical surface size (and
  `_mthid_kMTHIDPropertySurfaceWidth_mm` / `SurfaceHeight_mm` the same in millimetres), so
  the units/mm resolution libinput *requires* to classify our virtio device as a touchpad
  comes from the real hardware rather than a points→mm guess off AppKit's `deviceSize`.
  `MTDeviceGetGUID` / `GetDeviceID` / `GetFamilyID` likewise give a stable device identity
  to key a guest device on, and `MTDeviceCreateList` (not the `CreateDefault` the ObjC
  libraries use) covers an external Magic Trackpad alongside the built-in.
- **The MT device must be torn down across host sleep.** `OpenMultitouchSupport` stops the
  device on `NSWorkspaceWillSleepNotification` and restarts it on `NSWorkspaceDidWake` —
  a maintained library carrying that code is evidence the handle does not survive a sleep
  cycle. limina already has host-sleep plumbing (`docs/design/host-sleep-s2idle.md`); the
  MT teardown hangs off the same seam, and it must also release every guest contact slot,
  or the guest wakes with stuck fingers.
- **Probably no new TCC prompt; App Sandbox must be off.** `OpenMultitouchSupport`'s demo
  app ships an **empty** entitlements dict and its README's only requirement is that the
  App Sandbox be disabled — no Accessibility, no Input Monitoring. limina is already
  unsandboxed (it needs the hypervisor entitlement), so this may cost nothing. Which
  prompt, if any, actually appears is still a spike measurement; if one does, an ad-hoc
  signature is fatal to it (see `limina-tcc-adhoc-accessibility`).

### Suppression hypotheses, in order of plausibility

Each is a private-API lever whose *scope* — per-client connection or device-global — is
unknown and cannot be reasoned out; only the spike discriminates.

1. **`MTDeviceSetParserEnabled(dev, false)`** (with `MTDeviceGetParserEnabled` /
   `GetParserType` / `GetParserOptions`). The "parser" is the stage that turns contact
   frames into paths and gestures. The most direct candidate, and the one most likely to
   be per-connection — in which case it disables nothing for anyone else.
2. **`MTDeviceStop` / `MTDevicePowerSetEnabled(false)`.** Kills the device outright,
   which is semantically *close to what capture wants anyway* (under a grab the host has
   no business acting on the trackpad). Only useful if raw frames keep reaching our client
   while stopped — otherwise we have merely turned the trackpad off. The framework also
   exports the re-injection side (`MTDeviceDispatchScrollWheelEvent`,
   `DispatchRelativeMouseEvent`, `DispatchButtonEvent`, `DispatchMomentumScrollEvent`),
   which is what a full take-over would need to keep the *host* usable.
3. **Gesture configuration** — `_mthid_createGestureConfiguration`,
   `_mthid_appendBehaviorToGestureConfiguration`, `_mthid_serializeGestureConfiguration`,
   `MTSetGenericParameterValue`. Plausibly the programmatic form of the System Settings
   checkboxes, i.e. the thing BTT tells users to do by hand. Attractive because it is the
   one lever with public evidence of *working*; unattractive because it is global and
   persistent — flipping a user's system preference on grab and restoring it on release is
   a state we would own and could leak on a crash.
4. **A `.cghidEventTap`-location `CGEventTap` consuming gesture events.** We have
   *measured* that a session tap cannot see them (`docs/roadmap.md`, M8 polish); whether an
   HID-location tap sits upstream of WindowServer's recognizer is unmeasured. Zero private
   API if it works.

### If suppression works: what changes

Scope it to **captured / fullscreen mode only**, mirroring the keyboard grab — while
suppression is live the host loses its own trackpad gestures, which is acceptable only
under an explicit grab and must be released on every teardown transition the state
machine already handles. Then, under capture: all contacts forward, the Fn chord and the
whole synthetic-contact mechanism become unnecessary (keep them for seamless mode), the
MT device advertises `QUADTAP` and enough slots for real hands, and the guest gets true
pinch/rotate/swipe plus libinput's own palm rejection fed by real ellipse and pressure
data. Seamless mode keeps the ownership rule and the tablet exactly as designed.

### If suppression does not work

Take the raw stream as the *source* for the 2-finger contact pair anyway — richer and
lower-latency than `NSTouch`, and it answers the doc's open question about `NSTouch`
delivery to non-key windows by sidestepping it entirely (raw delivery is unconditional).
Ownership rule, Fn chord and synthesis all stand as written.

### Sequencing: the device and the source are independent

Nothing above blocks building the MT device. The **guest-side half** — the new
virtio-input touchpad, its slots and tracking IDs, `INPUT_PROP_BUTTONPAD`, the
`abs_info.res` the libkrun vtable already carries, the teardown state machine — can be
built and validated with AppKit `NSTouch` as its source, and it holds all the real risk:
whether libinput classifies the device as a clickpad, whether contacts release cleanly on
every transition, whether the result feels right in the guest.

The raw contact stream then swaps in underneath as a better **source** without changing
the device, and the suppression question only decides **how many fingers the device is
allowed to carry** — two, or everything under capture. Build the device first; the spike
decides how good it gets, not whether it works.

### Spike

`spikes/mt-raw-capture/` holds the measurement plan. Not yet run.

## Open questions / verification list

- Does AppKit deliver indirect `NSTouch` events to a non-key window under the cursor,
  the way it delivers scroll events? Don't assume — probe empirically. If not, MT
  forwarding effectively gains a key-window gate in practice.
- Whether libinput's size-based thumb/palm heuristics behave on the synthetic device;
  pick sane fuzz/flat. (The `res` derivation itself is answered if we take the raw path:
  `MTDeviceGetSensorSurfaceDimensions` — see §Alternative.)
- Host tap-to-click click synthesis timing vs our swallow window (does the click arrive
  after the touch sequence ends?).
- Whether momentum-phase scroll events reliably carry a marker tying them to the
  originating touch sequence (needed for the swallow window's tail).
- Guest libinput behavior when contacts always begin as a simultaneous pair (expected
  fine — indistinguishable from two fingers landing together — but verify scroll onset
  latency feels right).
