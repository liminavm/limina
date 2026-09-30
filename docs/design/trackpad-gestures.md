# Trackpad gestures: a guest-side multitouch device with strict contact ownership

Status: the guest touchpad, its `NSTouch` feed and the HID-level gesture tap are BUILT
(§What is built); two-finger scroll, pinch, taps and clicks work on the stock tier. The tap
takes guest three-finger sequences from macOS while four fingers stay macOS's (user-poked on
the default "three or four" setting). Two-finger system
gestures (smart zoom, the right-edge Notification Center swipe) are not taken. Ownership decided (§The ownership rule): the guest owns 2-
and 3-finger sequences in seamless mode and under capture alike, made possible by a measured
HID-level event tap that suppresses the host's gestures per finger count (§Raw multitouch
capture and gesture suppression). The companion quick win SHIPPED 2026-07-28: hi-res scroll
(f1a8e56) — see §Independent quick win.

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
- **Remote trackpads** (another Mac's, over Universal Control) cannot feed the device.
  They produce no raw contacts, and their forwarded `NSTouch`es have no device and no
  readable position (measured). Their gestures therefore stay on the host path: the swallow
  must count **local touches only** (touches with a device, or the local raw count), or a
  remote gesture that nothing forwards is simply lost.

## What is built

- **The device** — `limina_input::backends::TouchpadConfig` (worker) and the encoder
  `limina_input::touchpad::Touchpad` (host). Stock Fedora 44 (kernel 6.19.10, libinput 1.31)
  lists it as `Size: 125x77mm`, `Capabilities: pointer gesture`, `PROP=5`.
- **The size** — `crates/limina/src/hosttrackpad.rs` reads the default trackpad's surface
  (`MTDeviceGetSensorSurfaceDimensions`, `dlopen`ed) once per process and passes it to the
  worker as `--input-touchpad-size`.
- **The gesture tap** — `window/gesture_tap.rs`, an HID-level `CGEventTap` over the gesture
  types (18, 19, 20, 29, 30, 31, 32) that returns NULL while the policy says a guest-owned
  sequence reached three fingers (`TrackpadSeq::swallows_gestures`), for the whole sequence.
  An event it takes never reaches the app, so **while it is installed it is the touch source**
  (`InputState::on_tap_gesture`, the pointer's position hit-tested against the guest windows)
  and the local monitor ignores gesture events. Two fingers are left to macOS on purpose: its
  two-finger tap is the guest's right-click, and whether that survives the swallow was never
  measured. The Input menu's **Three-Finger Gestures in the VM** (on by default, remembered per
  VM) switches three fingers between the guest and macOS; the tap is created on the first tick
  with the switch on, needs Accessibility, and a menu toggle-on without it raises the prompt.
- **The feed** — the local event monitor takes `NSEventMask::Gesture`, and
  `InputState::on_gesture` reads `allTouches()` (local, non-resting, touching). AppKit attaches
  touches to those events **only when a view opts in**: the guest views set
  `allowedTouchTypes = .indirect` (`guestwindow.rs`); without it every gesture event arrived
  empty.
- **The policy** — `window/trackpad.rs`, pure and unit-tested: sequence ownership, the scroll
  dedupe, and the timing rules in §Dedupe and teardown.
- **The recordings** — `LIMINA_TRACKPAD_RECORD=<file>` writes every input the policy consumes
  (each gesture event's local touches, each trackpad click) as JSON lines. Recorded batteries
  of real hands live in `crates/limina/testdata/trackpad/` and are the policy's fixtures:
  `window::trackpad::recordings` replays each through the policy and checks, against
  libinput's tap rules (180 ms, 1.3 mm), that every click macOS recognised reaches the guest
  once and the touchpad adds none. `scripts/trackpad-oracle.sh <recording> <ssh-port>` judges
  the same replay with the **real** libinput in a booted guest (uinput clones of both devices,
  events at their recorded times). The two agree: immediate forwarding fails both with the
  same 71 guest taps on `battery-1`. The oracle also measures the touchpad's two-finger
  scrolls as a client sees them (events per scroll, the velocity GTK's kinetic scrolling
  computes at the stop, deltas against the scroll's direction). Its guest-side replay runs
  late by up to ~20 ms at times, so its jump and step counts are noisy; the unit tests apply
  libinput's rules exactly.
- **Diagnostics** — `LIMINA_POINTER_WIRE_TRACE` adds `dev=touchpad` writes, a `[TOUCH]` line
  per gesture event (all/local counts, in-view) and `[CLICKSRC]` per forwarded press.
  `spikes/mt-raw-capture/guest-evdev-log.py` logs the guest device's raw stream in wallclock
  microseconds that match the host's `[WIRE] t=` stamps event for event.

## Host side: touch source and device config

- **Source:** either will do; the device does not care which. `NSTouch` is what is built.
  - **The raw multitouch stream** (§Raw stream) gives positions, ellipses and pressure
    at ~124 Hz, device-global, plus the real surface size.
  - **AppKit indirect touches** — `NSView.allowedTouchTypes = .indirect`, then
    `touchesBegan/Moved/Ended`. Per finger: normalized `[0,1]` position, stable
    identity, phase, plus `deviceSize` in points.
  - The same `NSTouch` data can be read from the HID tap's gesture events with
    `NSEvent(cgEvent:).allTouches()`, whatever window is under the cursor. For local
    trackpads it matches the raw stream to ~0.001 (measured).
- **Device config (round one):** `EV_ABS` with `ABS_MT_SLOT` (3 slots),
  `ABS_MT_TRACKING_ID`, `ABS_MT_POSITION_X/Y` + legacy `ABS_X/Y`; `EV_KEY` with
  `BTN_TOUCH`, `BTN_TOOL_FINGER`, `BTN_TOOL_DOUBLETAP`, `BTN_TOOL_TRIPLETAP`,
  `BTN_LEFT`; `INPUT_PROP_POINTER` + `INPUT_PROP_BUTTONPAD`; fuzz on the position axes
  (§Dedupe and teardown). **`abs_info.res`
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
  for the whole sequence: from the first contact down until the last contact lifts, plus
  the momentum tail. That stops system-level recognition. Measured:
  - Spaces and Mission Control at three fingers.
  - Notification Center's right-edge swipe and smart zoom at two fingers
    (`RESULTS.md` §hidtap-2).
  - **Own the sequence, not the instant.** The probe's rule — swallow while exactly N
    contacts are down *now* — passed 1–2 gesture events whenever fingers lifted a few ms
    apart; owning the whole sequence closes that.
- **limina's own view drops what reaches it**: cooked `ScrollWheel`, magnify and
  tap-generated clicks (e.g. host two-finger-tap right-click). Suppress `emit_scroll`
  entirely for trackpad-sourced scrolls while the MT device owns the sequence; otherwise
  the guest gets the gesture twice.
- **Deciding the count:** use the sequence's *peak* count. Counts ramp while fingers land,
  and a sequence that ever reaches four is the host's from then on.

**Timing rules, each measured on the stock tier:**

- **Never show libinput a touch jump.** The guest has no timestamps from us (its kernel stamps
  each frame on arrival), and libinput discards a frame whose contact moves more than 20 mm,
  or 7 mm more than in its last frame, per 12 ms (`tp_detect_jumps`) — logging `kernel bug:
  Touch jump detected and discarded`, and losing that motion from the gesture. Three rules
  keep every frame under it, and `no_battery_shows_the_guest_a_touch_jump` checks them against
  that rule on every recording:
  - **Motion frames are paced ≥ 10 ms apart** (`MIN_FRAME_INTERVAL`), the newest sample
    going out — unless folding the held sample into it would make a frame libinput reads
    as a jump (the policy keeps each contact's last speed and applies the rule with 6 mm
    of margin), in which case the held sample goes first and the newest follows. Samples
    arrive every ~16 ms through the local monitor, some in back-to-back pairs ~0.3 ms apart
    that each carry half a step (merging them is right); through the HID tap every 7–8 ms
    or 16 ms.
  - **The frame after a moved commit's landing carries the committing sample**, never a
    newer one, spaced by its distance at 6 mm per 12 ms (`COMMIT_SPEED_MM_PER_12MS`) and at
    least the pacing. It holds the whole distance moved before the commit — 3–8 mm on a
    flick — and a newer sample, or a shorter gap, read as a jump and cost the flick its start.
  - **A finger landing or lifting is never held back, but the fingers already down stay
    where the guest last saw them** in that frame; their motion follows at the pacing. A
    third finger landing 2–6 ms after a motion frame otherwise carried the others' 4–5 mm
    with it, and three-finger swipes lost their start.
- **The position axes declare fuzz 16 (0.16 mm, `TOUCHPAD_FUZZ`).** libinput turns a
  touchpad's fuzz into its own hysteresis, whose output trails the finger by the margin: a
  reversal smaller than it is absorbed. With none, a finger coming to rest scrolled the
  content back and forth by fractions of a pixel (`spikes/scroll-wobble/`). A finger that
  creeps on in the same direction after stopping still moves the content, in small nudges
  after a pause.
- **Taps and clicks are macOS's alone; the guest never sees a touch it could read as a
  tap.** Two recognizers see the same fingers: macOS turns taps and clicks into mouse clicks,
  and libinput would read its own taps from the contacts. Their events arrive on separate,
  unordered streams — a click can reach limina 3–5 ms *before* the touches it belongs to,
  touches vanish while the pad is pressed and reappear after, and macOS's tap click lands
  287–310 ms after the fingers lift (it waits out the double-tap window; measured on
  `battery-1`). Routing each click to one recognizer was therefore a race, and lost it
  visibly (a menu opened and closed by two right-clicks). Instead a guest-owned sequence's
  contacts reach the guest only once it **commits**: a finger moved ≥ 3 mm (`COMMIT_MOVE_MM`;
  the guest then gets the landing positions first and the motion after, past libinput's
  1.3 mm tap threshold), or two fingers stayed down 200 ms (`COMMIT_HOLD`; the guest then
  holds the touch at least 200 ms, `TAP_GUARD`, past libinput's 180 ms tap timeout). Every
  click goes to the tablet exactly as macOS recognised it, so the Mac's tap-to-click setting
  governs two-finger taps over the VM, as it does one-finger ones. That includes its latency:
  macOS delivered each one-finger tap's click 220–250 ms after the lift (`battery-2`, with
  tap-to-drag on), and limina forwards it as it arrives.
- **Lift the moment every finger is up; wait 50 ms on a drop to one** (`THIN_GRACE`). A
  kinetic scroll's velocity is read from the motion just before the lift, so fingers held
  still in the guest past the real lift read as a stop and kill the momentum. AppKit's count
  does flicker mid-gesture, but in the recordings only to one finger (dips of 0.4–155 ms),
  never to none — so a drop to one waits out the grace and a drop to none lifts at once. The
  motion the pacing held back goes out before the lift: dropped, a flick that loses a finger
  just after committing shows the guest a still touch, which is a tap. A resting finger
  (macOS's reading of a thumb) does not start or widen a sequence, but one the guest already
  holds keeps counting: macOS also marks fingers resting when they merely hold still.
- **A gesture event with no touches at all says nothing about the fingers.** At the HID level
  every other gesture event of a two-finger scroll carries none (1754 of 3763 in one poke;
  the local monitor saw one in a session). Read as a lift, each one lifted and re-landed the
  guest's fingers, and no scroll ever committed. A real lift carries its touches, ended.
- **Three fingers need the HID tap.** When macOS claims a three-finger swipe (the default
  setting), AppKit stops attaching touches to the app's gesture events: of ~8 000 gesture
  events in one poke, 57 carried three touches. The guest cannot see the swipe until the
  host's recognizer is suppressed.

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


- Does AppKit deliver indirect `NSTouch` events to a non-key window under the cursor,
  the way it delivers scroll events? Don't assume — probe empirically. If not, MT
  forwarding effectively gains a key-window gate in practice.
- Whether libinput's size-based thumb/palm heuristics behave on the synthetic device.
  (The `res` derivation itself is answered if we take the raw path:
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
  trackpads too (measured, `RESULTS.md` §hidtap-3ns). But the MT device cannot serve
  remote input — no contacts, and its `NSTouch`es carry no device or position — so limina
  must *not* swallow remote gestures.
- Only a tap that is already installed and filters per event on the live count is
  measured. A tap *installed* at count determination would have to beat the recognizer
  (Dock transition windows appeared ~180–280 ms after touchdown), so don't build that one.
- Which of the swallowed event types actually matter to the HID-tap suppression: the spike
  swallowed seven, and type 29 (`NSEventTypeGesture`) streams at ~90/s under *any*
  contact, one finger included.
- Whether momentum-phase scroll events reliably carry a marker tying them to the
  originating touch sequence (needed for the swallow window's tail).
- Guest libinput behavior when contacts always begin as a simultaneous pair (expected
  fine — indistinguishable from two fingers landing together — but verify scroll onset
  latency feels right).
