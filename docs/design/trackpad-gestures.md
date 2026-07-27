# Trackpad gestures: a guest-side multitouch device with strict contact ownership

Status: DESIGN 2026-07-28. Not implemented. Companion quick win (hi-res scroll) can land
independently and first.

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

## Independent quick win: hi-res scroll

Regardless of the MT device: switch `emit_scroll` to `REL_WHEEL_HI_RES` /
`REL_HWHEEL_HI_RES` carrying the actual pixel deltas. Fixes the quantized-scroll feel
gap on its own; macOS momentum-phase events provide kinetic for free. (The MT device
supersedes it for trackpad scrolls but hi-res still serves mice and any swallowed-path
fallback.) Land this first.

## Open questions / verification list

- Does AppKit deliver indirect `NSTouch` events to a non-key window under the cursor,
  the way it delivers scroll events? Don't assume — probe empirically. If not, MT
  forwarding effectively gains a key-window gate in practice.
- Exact `res` derivation from `deviceSize` (points → mm) and whether libinput's
  size-based thumb/palm heuristics behave on the synthetic device; pick sane
  fuzz/flat.
- Host tap-to-click click synthesis timing vs our swallow window (does the click arrive
  after the touch sequence ends?).
- Whether momentum-phase scroll events reliably carry a marker tying them to the
  originating touch sequence (needed for the swallow window's tail).
- Guest libinput behavior when contacts always begin as a simultaneous pair (expected
  fine — indistinguishable from two fingers landing together — but verify scroll onset
  latency feels right).
