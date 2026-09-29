# Trackpad gestures: a guest-side multitouch device with strict contact ownership

Status: DESIGN. MT device not implemented. The companion quick win SHIPPED 2026-07-28:
hi-res scroll (f1a8e56) — see §Independent quick win. Ownership decided (§The ownership
rule): the guest owns 2- and 3-finger sequences in seamless mode and under capture alike,
made possible by a measured HID-level event tap that suppresses the host's gestures per
finger count (§Raw multitouch capture and gesture suppression).

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

macOS's WindowServer recognizes multi-finger gestures (Spaces, Mission Control, App
Exposé, Launchpad) at the system level and acts on them whichever app receives the
touches. Unclaimed multi-finger contacts become ordinary cooked scroll. Forwarding
contacts to the guest therefore also needs a way to stop the host from acting on them. An
**HID-level event tap** does that per finger count (measured — §Raw multitouch capture and
gesture suppression). With it, the partition is:

| physical contacts | owner | delivered to guest as |
|---|---|---|
| 1 finger | **host** (tablet, host pointer ballistics) | `ABS_X/Y` tablet motion, as today |
| 2 fingers | **guest** — scroll, pinch, rotate | real MT contacts on the new device |
| 3 fingers | **guest** — swipes (GNOME ≥40 binds everything to 3) | real MT contacts on the new device |
| 4+ fingers | **host** — Spaces, Mission Control stay usable | nothing |

- **It applies in seamless mode and under capture alike**, gated on the cursor being over
  the VM view.
  - The cost is that host gestures at 2 and 3 fingers (three-finger Spaces swipes on the
    default "three or four" setting, the right-edge Notification Center swipe) do nothing
    over the VM.
  - Four fingers stay the host's, so every host gesture remains reachable. Revisit if this
    causes friction in use.
- **Single-finger sequences are never forwarded**, so the MT device never drives the guest
  cursor and cannot fight the tablet for it. That restriction is what makes a guest-side
  touchpad compatible with the seamless host-cursor = guest-cursor model.
  - Full MT forwarding (1-finger motion included) is a possible *hard-capture* refinement
    later.
- **Four-finger guest gestures** (KDE Plasma binds some) are out of scope. If they are
  ever wanted, a chord (e.g. Fn) that maps a physical 3-finger sequence to four
  synthetic contacts is the route; the host keeps its physical four.
- **Remote trackpads** (another Mac's, over Universal Control) carry no contacts, so they
  cannot feed the device. Their gestures stay on the host path. See the open question on
  their `NSTouch` data before deciding otherwise, because swallowing a remote gesture
  that nothing forwards just loses it.

## Host side: touch source and device config

- **Source:** either will do; the device does not care which.
  - **The raw multitouch stream** (§Raw stream) gives positions, ellipses and pressure
    at ~124 Hz, device-global, plus the real surface size.
  - **AppKit indirect touches** — `NSView.allowedTouchTypes = .indirect`, then
    `touchesBegan/Moved/Ended`. Per finger: normalized `[0,1]` position, stable
    identity, phase, plus `deviceSize` in points.
- **Device config (round one):** `EV_ABS` with `ABS_MT_SLOT` (3 slots),
  `ABS_MT_TRACKING_ID`, `ABS_MT_POSITION_X/Y` + legacy `ABS_X/Y`; `EV_KEY` with
  `BTN_TOUCH`, `BTN_TOOL_FINGER`, `BTN_TOOL_DOUBLETAP`, `BTN_TOOL_TRIPLETAP`,
  `BTN_LEFT`; `INPUT_PROP_POINTER` + `INPUT_PROP_BUTTONPAD`. **`abs_info.res`
  (units/mm) is mandatory** — libinput refuses/degrades touchpads without resolution;
  take it from `MTDeviceGetSensorSurfaceDimensions` (124.8 × 76.8 mm on the built-in →
  100 units/mm at 0.01 mm units), or derive it from `deviceSize`. The libkrun vtable already carries `res`
  (`third_party/libkrun/include/libkrun_input.h:103-109`), so this may need no libkrun
  change beyond the new config backend. No `QUADTAP`, no 4th slot: four fingers are the
  host's, and the device advertises only what can actually arrive.
- **Gating:** forwarding gates on **cursor over the VM view** (like scroll routing), not
  on key-window status — orthogonal to the soft *keyboard* grab, which stays
  key-gated. Applies in soft/seamless mode and capture alike.

## Dedupe and teardown (the state machine)

While a forwarded 2- or 3-finger sequence (plus its momentum tail) is in flight, the host
must not also act on it. Momentum-end events land up to ~1 s after the last finger lifts
(measured).

- **The HID tap swallows the host's gesture events** (types 18, 19, 20, 29, 30, 31, 32)
  for the sequence. That stops system-level recognition: Spaces and Mission Control at
  three fingers are measured; the right-edge two-finger swipe is not yet (§Open
  questions).
- **limina's own view drops what reaches it**: cooked `ScrollWheel`, magnify and
  tap-generated clicks (e.g. host two-finger-tap right-click). Suppress `emit_scroll`
  entirely for trackpad-sourced scrolls while the MT device owns the sequence; otherwise
  the guest gets the gesture twice.
- **Deciding the count:** use the sequence's *peak* count. Counts ramp while fingers land,
  and a sequence that ever reaches four is the host's from then on.

Teardown: on *any* transition — cursor leaves the view, window loses key, capture
toggles, a fourth physical finger lands (the host is taking over), or the fingers lift — release every guest slot cleanly (`tracking_id` −1, `BTN_TOUCH` up, SYN) so the
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
- macOS *three-finger drag* (an Accessibility setting, `TrackpadThreeFingerDrag`) claims
  three fingers that the guest now owns. When it is on, limina should leave three fingers
  to the host rather than take them: read the setting, as with the swipe prefs.

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

## Raw multitouch capture and gesture suppression

Two independent findings, both measured on the dev Mac (M1 Max built-in trackpad, macOS
26.6.2; `spikes/mt-raw-capture/RESULTS.md`):

1. **A better touch source.** macOS's private `MultitouchSupport.framework` publishes the
   trackpad's raw contact stream (every finger's position, ellipse, pressure and density,
   at ~124 Hz) to any unsandboxed client. That is strictly richer than AppKit `NSTouch`.
2. **Host gestures can be suppressed**, by a public API: an HID-level `CGEventTap` that
   swallows gesture event types. Reading raw contacts suppresses nothing by itself.

### Suppression levers (measured 2026-09-29)

| lever | host 3/4-finger gestures | pointer, scroll, clicks, haptics | outlives a crash? |
|---|---|---|---|
| **HID tap** swallowing gesture types | **inert** | **unaffected** | no — dies with the process |
| `MTDeviceSetParserEnabled(false)` | inert | **all dead** (no haptics either) | **yes** — device-global driver state |
| `MTDevicePowerSetEnabled` | — | — | `kIOReturnUnsupported` on this trackpad |
| `MTDeviceStop`, `_mthid_*GestureConfiguration` | not run — moot after the HID tap; the latter is global persistent state anyway | | |

- **The HID tap is the lever.** It is `CGEventTapCreate(kCGHIDEventTap, head, default, …)`
  over event types 18, 19, 20, 29, 30, 31, 32 (rotate, begin/end gesture, gesture, magnify,
  swipe, smart magnify), returning NULL for them. With it active:
  - Spaces, Mission Control and App Exposé swipes did nothing, both with macOS on "three
    or four fingers" (3- and 4-finger swipes inert) and on "four" (4-finger inert).
  - Pointer motion, cooked two-finger scroll, physical click, force click and haptics all
    kept working.
  - It needs Accessibility, the same grant as limina's existing session-level capture tap
    (`crates/limina/src/window/capture_tap.rs`). The spike ran from a terminal, so TCC
    attributed it there; confirm in the app.
  - **It can be selective.** Kept installed and filtering per event on the raw stream's live
    contact count, swallowing only while exactly three fingers are down and the sequence
    never reached four, it made 3-finger swipes inert. With the default "three or four"
    setting, 4-finger Space and Mission Control swipes kept working, including slowly
    landed ones (`RESULTS.md` §hidtap-3). So the guest can get three fingers with no change
    to the user's settings.
  - **Keying on AppKit's count works too, and reaches remote trackpads.** Deciding on
    `NSEvent(cgEvent:).allTouches().count` of the gesture events gave the same split for
    the local trackpad and for trackpads on another Mac over Universal Control. It uses the
    sequence peak, with a sequence ending after 150 ms of zero counts
    (`RESULTS.md` §hidtap-3ns). Suppression therefore does not need the private framework;
    only the guest MT device does.
  - This does not contradict the M8 finding (`docs/roadmap.md`): that was about a
    *session* tap. That the session tap sits downstream of the recognizer is inferred from
    the two results, not measured side by side.
- **parser-off works too but is rejected.**
  - It is a kernel-driver request (message 0x11 via `MTDeviceIssueDriverRequest`, read off
    the disassembly), so it is device-global.
  - It survived an unclean kill: a fresh process read `parser=false` until explicitly
    restored.
  - It kills host pointer, clicks and haptics along with the gestures.
  - Raw frames keep flowing under it. The spike's `--arm restore` is the repair tool if a
    probe ever leaks it again.
- **Clicks are visible in the raw stream.** Contact pressure reads ~20–50 when resting,
  ~300–400 on a physical click and ~650 on a force click. So a guest `BTN_LEFT` can come
  from the raw source if that is ever wanted. Under the HID tap the host's own click path
  keeps working anyway.

### The cheap unlock: give macOS four fingers, take three

macOS's Mission Control / Spaces / App Exposé swipes are individually configurable to three
*or* four fingers. Set them to four, and three-finger contacts are no longer claimed by
the host. Three fingers is exactly the count GNOME ≥40 binds everything to. With the HID
tap this is no longer required. It remains the path that needs no Accessibility grant
(three fingers only; two-finger scroll still needs limina's in-view swallow).

The relevant defaults, in `com.apple.AppleMultitouchTrackpad` (built-in) and
`com.apple.driver.AppleBluetoothMultitouch.trackpad` (external Magic Trackpad) — `2` is
enabled, `0` disabled:

    TrackpadThreeFingerHorizSwipeGesture   ← 0 when macOS is on four fingers
    TrackpadThreeFingerVertSwipeGesture    ← 0 when macOS is on four fingers
    TrackpadFourFingerHorizSwipeGesture    ← 2; macOS keeps four
    TrackpadFourFingerVertSwipeGesture     ← 2
    TrackpadThreeFingerDrag                ← must be 0, or Accessibility claims 3 fingers
    TrackpadThreeFingerTapGesture          ← must be 0

- **limina reads these and never writes them.** They are global, persistent user settings.
  Detect the state and tell the user which counts the host has claimed; make forwarding
  configurable by finger count.
- **They are a live oracle.** `UserDefaults(suiteName:)` reads the new values right after
  a change in System Settings, with no logout, and the behavior has already changed.
  "Four" writes `ThreeFinger*Swipe = 0` and leaves `FourFinger*Swipe = 2`; "three or
  four" is both at `2`.
- **Freed three-finger contacts become ordinary cooked scroll, momentum included**, in the
  app under the cursor. A forwarded 3-finger group therefore needs the same scroll swallow
  as the 2-finger pair (§Dedupe and teardown).
  - The cooked event carries no finger count, so only the touch source's contact count
    can tell a 3-finger scroll from a 2-finger one.
  - When the HID tap swallowed gesture types and the host still claimed three fingers, no
    such scroll appeared.

### Raw stream: what is established

- **Reading raw contacts works from an ordinary unsandboxed binary**, with no kext, no
  daemon and no TCC prompt observed. The sequence is `MTDeviceCreateList` →
  `MTRegisterContactFrameCallback` → `MTDeviceStart(dev, 0)`, all via `dlopen`/`dlsym` on
  `/System/Library/PrivateFrameworks/MultitouchSupport.framework/MultitouchSupport`; keep
  `dlopen` so a vanished symbol degrades gracefully.
- **Reference implementations**, under `~/Projects/`:
  - **`OpenMultitouchSupport`** (Kyome22, MIT) is the one to read. It is maintained, and
    `Framework/OpenMultitouchSupportXCF/OpenMTInternal.h` is the best declaration of the
    private API.
  - **`M5MultitouchSupport`** is its 2015 ancestor.
  - **`GutchinTouchTool`** is the Swift/`dlopen` reference for binding *mechanics*; its
    data model is wrong past offset 40.
- **Delivery is device-global, not window-scoped.** The callback fires for every touch
  regardless of focus, key window or cursor location; the gate is ours to impose.
- **`MTTouch` is 96 bytes** with `normalizedPosition` as an `MTVector` (position +
  velocity). This was measured: stride 96, and every position fell in [0,1]. Where they
  suffice, prefer the opaque-handle accessors (`MTRegisterPathCallbackWithRefcon` +
  `MTPath_*`, `MTContact_getEllipse*`), which a layout change cannot shift. That
  cross-check was not run.
- **The device answers `abs_info.res`.** `MTDeviceGetSensorSurfaceDimensions` gives
  12480 × 7680 in 0.01 mm (124.8 × 76.8 mm) on the built-in trackpad → report positions
  in 0.01 mm with `res` = 100 units/mm. Sensor grid is 24 × 18, family 0x6c.
  `MTDeviceCreateList` also covers an external Magic Trackpad.
- **Tear the MT device down across host sleep.** `OpenMultitouchSupport` recreates it
  around `NSWorkspaceWillSleep`/`DidWake`. Hang it off limina's host-sleep seam
  (`docs/design/host-sleep-s2idle.md`) and release every guest contact slot there. This is
  unmeasured.

### Sequencing: the device and the source are independent

Nothing above blocks building the MT device.
- **The guest-side half holds all the real risk**: the new virtio-input touchpad, its
  slots and tracking IDs, `INPUT_PROP_BUTTONPAD`, the `abs_info.res` the libkrun vtable
  already carries, and the teardown state machine. It can be built and validated with
  either source. The risk is whether libinput classifies the device as a clickpad, whether
  contacts release cleanly on every transition, and whether the result feels right.
- **The source swaps in underneath without changing the device.**
- **The device carries two or three contacts** (§The ownership rule).

### Spike

`spikes/mt-raw-capture/`: `mtprobe.swift` (arms `baseline`, `parser-off`, `stop`,
`power-off`, `hidtap`, `restore`), the measurement plan in `README.md` and the numbers in
`RESULTS.md`. The probe detects host gesture actions without a human watching: the
Space-change notification plus the Dock's window layers.

## Open questions / verification list

- Does the HID tap stop two-finger system gestures (the right-edge Notification Center
  swipe, smart zoom, Look Up) when swallowing at count 2? Only 3- and 4-finger swipes are
  measured.
- Do the `NSTouch`es on Universal Control-forwarded gesture events carry positions? If so,
  remote trackpads could feed the device too, and swallowing their 2/3-finger gestures
  would stop losing them.

- Does AppKit deliver indirect `NSTouch` events to a non-key window under the cursor,
  the way it delivers scroll events? Don't assume — probe empirically. If not, MT
  forwarding effectively gains a key-window gate in practice.
- Whether libinput's size-based thumb/palm heuristics behave on the synthetic device;
  pick sane fuzz/flat. (The `res` derivation itself is answered if we take the raw path:
  `MTDeviceGetSensorSurfaceDimensions` — see §Raw multitouch capture.)
- HID-tap coverage beyond the swipes exercised: 4/5-finger pinch (Launchpad, show
  desktop), the two-finger right-edge swipe (Notification Center), and App Exposé opened
  from a neutral state. Every 3-finger swipe down in the runs closed Mission Control
  rather than opening App Exposé.
- A trackpad on another Mac, used over Universal Control, reaches this Mac as
  already-recognized gesture/scroll/magnify events with no raw contacts. The host Dock
  acts on them, and they carry pid 0 like local input (`RESULTS.md` §hidwatch). The
  swallowing tap suppresses them too, and `NSEvent(cgEvent:).allTouches().count` reports
  their finger count (3 and 4 measured). A selective tap keyed on that count covers remote
  trackpads too (measured, `RESULTS.md` §hidtap-3ns). The MT device cannot serve remote
  input, which has no contacts.
- Only a tap that is already installed and filters per event on the live count is
  measured. A tap *installed* at count determination would have to beat the recognizer
  (Dock transition windows appeared ~180–280 ms after touchdown), so don't build that one.
- Which of the swallowed event types actually matter to the HID-tap suppression: the spike
  swallowed seven, and type 29 (`NSEventTypeGesture`) streams at ~90/s under *any*
  contact, one finger included.
- Host tap-to-click click synthesis timing vs our swallow window (does the click arrive
  after the touch sequence ends?).
- Whether momentum-phase scroll events reliably carry a marker tying them to the
  originating touch sequence (needed for the swallow window's tail).
- Guest libinput behavior when contacts always begin as a simultaneous pair (expected
  fine — indistinguishable from two fingers landing together — but verify scroll onset
  latency feels right).
