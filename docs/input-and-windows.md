# Input and windows: what the user points at, and what points back

The map for everything between a macOS event and a guest evdev event, and between a guest scanout
and a pixel on a panel. It is the counterpart of `docs/graphics.md`: that one owns *how a frame is
rendered and presented*, this one owns *which window shows it, where the pointer is, and who owns
the keyboard*. Read this before changing anything under `crates/limina/src/window/`.

The single most expensive mistake in this area is treating any one of these as a detail of another:
the **window**, the **guest connector it shows**, the **coordinate space the pointer lives in**, and
**whether the pointer is captured**. They are four independent facts. A change that assumes two of
them move together works until the second display, the notch, or the grab arrives — and then fails
somewhere far from the change.

## 1. The pieces

| file | owns |
|---|---|
| `window/mod.rs` | the primary `NSWindow` + its role (key, menu, close, lifecycle, persistence), the render/apply tick, the display control plane, wiring |
| `window/guestwindow.rs` | the per-window presentation core every guest window is made of: layer wiring, the resolve rule + resurface recovery, frame cache, shown-ack, letterbox layer write, inset learn, the `extend` strip + its seed, the capture-cursor walk |
| `window/windows.rs` | the guest-window collection: primary role + one window per other connector |
| `window/displays.rs` | the **slot table**: which host panel owns which guest connector |
| `window/hostdisplay.rs` | a host panel described as the EDID the guest should see |
| `window/present.rs` | per-slot present state shared with the worker reader thread |
| `window/fit.rs` | pure letterbox geometry + the inverse mapping the pointer needs |
| `window/arrangement.rs` | the arrangement relay's geometry, and the guest's own layout **report** — the absolute device's mapping and the edge-pressure filter read it |
| `window/input.rs` | `NSEvent` → evdev; the host cursor's shape; the capture toggle |
| `window/capture_tap.rs` | the session-level `CGEventTap` that makes capture reliable |
| `window/grab_policy.rs` | *policy*: the grab, its releases, the chrome reveal, `WindowFacts` — pure, unit-tested, no AppKit |
| `window/warp.rs` | the **warp broker**: the one owner of cursor warps, each asserted to land where it aimed |
| `window/cursor.rs` | the guest cursor, in both of its two presentations |
| `window/overlay.rs` | the suspend/resume scrim, and the notch **strip** carrier |

**The structural rework is landed** (`docs/design/input-windows-restructure.md`, Moves A/D/B/C:
the GuestWindow inversion, one gate-geometry rule, the ownership snapshot + pure policy engine,
the warp broker). The doc remains the rationale record — read it before adding any per-window
mechanism, grab/reveal decision site, or warp, so new code lands on the shape the moves built.

Design docs, all still current: `docs/design/fullscreen-pointer-grab.md` (the grab and its
release barrier),
`docs/design/display-modes.md` (`host | dynamic | WxH`, and `notch = avoid | extend`),
`docs/design/stable-edid-hotplug.md` (identity, hotplug, HiDPI), `docs/design/display-cutouts.md`
(exploratory; where `notch` would eventually go).

## 2. A window is not a display is not a slot

- A **slot** is a virtio-gpu scanout, fixed in number at boot (`num_scanouts` is config-space state
  read once at probe). Every display a VM may ever show exists from boot as a *disconnected*
  scanout; lighting one up is a `connected = true` push, never an allocation.
- A **panel owns a slot permanently**, keyed on `hostdisplay::panel_key`. mutter identifies a
  monitor by connector name *and* vendor/product/serial, and the connector name is the slot — so a
  panel that were to land on slot 0 in one session and slot 1 in the next would be *two different
  monitors* to the guest, each with its own saved arrangement. See `window/displays.rs`.
- **Fullscreen lights up the other panels only by request.** The Displays menu's "Use Other
  Screens When Fullscreen" (off by default, persisted per VM beside the per-panel switches)
  is what turns fullscreen into `FullscreenAll`; without it, fullscreen occupies exactly the
  window's own panel, like any window. The decision is `displays::presentation_for`.
- **Firmware paints slot 0 and only slot 0.** EDK2's virtio-gpu GOP driver hardcodes head 0, so
  until the guest's own driver takes over, slot 0 must be the connected one whatever panel the
  window is on.
- The **primary window** is the one that owns the keyboard, the menu, and the app's lifecycle. It is
  not "slot 0's window" — it shows whichever slot its panel owns. Every other connected slot gets a
  `SecondaryWindow`. Anything written as "slot 0" that means "the primary" is a bug in waiting; use
  `primary_slot`.
- The **guest is told where each connector sits** (the arrangement relay, `window/arrangement.rs`):
  positions in predicted guest-logical units ride `GET_DISPLAY_INFO` rects and surface in the
  enhanced kernel as the DRM `suggested X`/`suggested Y` + `hotplug_mode_update` connector
  properties, which mutter honors only when it has no stored config for the monitor set. The whole
  set must be adjacency-exact and in place *before* a connect's hotplug lands — in-place position
  pushes go out first, the arriving slot's position rides its connect, and the emitted set is
  pre-validated against mutter's own rules (emit all connectors clean, or nothing and keep the
  guest's linear default).

## 3. The coordinate spaces, and the conversions between them

Five spaces, and almost every pointer bug in this area has been a value used in the wrong one.

1. **AppKit view points** — bottom-left origin, per window. What `NSEvent` gives you.
2. **The fit rect** (`window/fit.rs`) — the letterboxed sub-rectangle of the view that the guest
   picture actually occupies. The pointer gate and the pointer mapping must use the *same* fit, or
   the gate and the emitter disagree about where the pointer is and motion silently vanishes.
   Under `notch = avoid` on a covered panel, read the **layer**'s rect, not the view's bounds —
   they differ by the camera-housing band.
3. **Unit coordinates within one display** — `0.0..=1.0`, top-left origin (`unit_through_fit`).
4. **The guest's desktop** (`window/arrangement.rs`, the report) — where each connector sits
   relative to the others, in the compositor's **logical** units. **The guest owns this
   arrangement and we cannot infer it**, so `limina-agent-session` reports the compositor's own
   logical rects over the
   control plane (`zxdg_output_v1`, falling back to `wl_output` mode ÷ integer scale — portable
   across mutter/KWin/wlroots) and the host maps through them. The host takes the report
   last-writer-wins from any control channel; what makes that correct is guest-side
   arbitration (`layout_gate.rs` in the helper): **only the session whose uid owns the seat's
   active logind session reports** (every graphical session runs a helper — the greeter's
   too), a held layout is re-sent on activation, and an absent logind fails open. Without an
   agent there is no layout, and **the host does not guess one**: each window maps its content
   straight onto the whole range (`arrangement::abs_through_report`) — exact for one display,
   and for two the stock tier has no correct mapping to offer. That is the documented stock
   floor, made loud rather than silently wrong by the guest-echo check (§5).
5. **The absolute device range** — `0..=ABS_MAX`, spread by the compositor over the **whole**
   desktop. This is the one that makes multi-display hard: a window can no longer answer "where is
   the pointer" by itself, because its full sweep is only *its share* of the range.

**The invariant:** a point becomes `view → fit → unit → range`, with the unit placed in the
guest's reported layout when there is one. The no-report mapping assumes "there is exactly one
display" — the only layout the host may assume, and it assumes it by not guessing a second;
which display the guest actually put the cursor on is then learned back from its echo.

## 4. Two pointer modes, and they present the cursor differently

This is the distinction most easily missed, because **fullscreen enters capture on its own** — so
any test done fullscreen is a test of the captured path, whatever it was meant to test.

**Uncaptured.** The macOS pointer moves normally and *wears* the guest's cursor image as an
`NSCursor` (`cursor.rs::apply_cursor`, `input::HostCursor`). Motion is gated to the guest content
and forwarded as absolute positions. The **scale** of the sprite comes from the window the pointer
is over — that is where it is drawn — but the **shape** comes from whichever slot has the cursor
plane enabled (`cursor::shape_slot`), which is routinely a different display: the guest has one
cursor on one CRTC, and it need not be under the host pointer. Reading only the pointer's slot made
every `cursorhide` for the display the guest's cursor *left* dress the host pointer in the blank —
an invisible pointer with no way to find it. The blank means "the guest is showing no cursor
anywhere", nothing less.

**Where an uncaptured position goes is measured, not assumed** (`window/absfit.rs`). The guest
spreads its one absolute device over the bounding box of every monitor it has, so the value we send
names a point on the whole desktop: `logical_x = value / ABS_MAX × union_width + …`. Sending a
window's own unit position straight to the range is exact for one display, whose share *is* the
range, and wrong for both once there are two — measured on the two-panel rig, a pointer at the
BenQ's centre put the guest's cursor 88% across it, and past 57.5% of *either* window the guest's
cursor was on the other monitor. Nothing about the union, the offsets or the guest's scales is
needed separately: per slot the whole relation is one line per axis in the value sent,
`pixel = a·u + b`, so placing the cursor on a pixel is `u = (pixel − b) / a`. The lines are fitted
from the guest's own cursor echo — every send is a sample — and re-fitted whenever three samples in
a row miss, because a monitor **repositioned** or its **scale** changed produces *nothing* on the
virtio-gpu wire (a fractional scale on a fixed-mode connector leaves the scanout byte-identical),
while connect/disconnect/modeset arrive as `surface`/`scanoutgone` and drop every slot's line at
once (one slot's mode is a term in every other slot's). Precedence: the guest's reported
arrangement when there is one (exact and immediate), else the fitted lines, else identity — which
is also what a stock guest gets until the fit converges.

To avoid waiting for the user to sweep far enough by accident, the mapping is also **probed**
deliberately (`InputState::probe_mapping`, `absfit::PROBE_SWEEP`): ten positions spanning the
range, each waiting only for the guest's reply, run the moment a second display makes the mapping
worth knowing. That is what makes gaining a display recover immediately instead of leaving
whichever slot the cursor is not on unmeasured.

**A sweep does not wait for a grab** (`absfit::probe_may_start`), and must not be made to: the
mapping it learns is the *uncaptured* pointer's, so gating it on capture learns it last where it
is needed first. It once did, and that was a deadlock rather than a delay — the sweep read its
restore position out of state only captured motion ever wrote, so the first stroke on a new
two-display session necessarily went through the identity mapping, landing on the wrong display.
What a sweep does wait for: a button-free pointer (mid-drag it would drag the guest's content
across its desktop), a rest since the last one, so a guest showing no cursor — every step
unanswered, the want still standing — cannot spin the tick, and **a gap in the user's own
movement**, because the moment a sweep is most wanted is the moment the hand is busiest.

**Only the interior of a display is sampled.** A pointer the guest is *clamping* measures
nothing: ride a desktop edge and the sends keep changing while the echoed pixel sits at the
boundary, a run of samples that vary in `u` and not in `pixel`. The mapping is linear only in the
interior, so a clamped echo is evidence that the axis has run out of display, not evidence about
the line. Fed in, such a run contradicts a correct line, reseeds a fit too flat to state, and
costs the slot its mapping — and a slot with no mapping is exactly what asks for a sweep, so
edge-riding *summoned* the sweeps that then interrupted it. Hence also: **a line is never given
up for nothing.** A dissent too bunched to fit keeps the line it doubts and stays pending, so the
next contradiction is judged on a wider base; wrong-but-stable beats none.

Two rules keep a sweep from being mistaken for the hand. The captured cursor **does not follow the
guest's cursor while a sweep is in flight**: that cursor is ours for the moment, and following it
warps the park across displays, reads as the pointer leaving the window, and drops the grab. And
the pointer is put back by **re-placing where the user left it through the mapping as it now
stands** — never by replaying a device number saved beforehand, which is a number in the mapping
the sweep just replaced, i.e. a teleport wearing a restore's clothes.

**Captured** (`Cmd-Ctrl-G`, and automatically on fullscreen). The host cursor is hidden and frozen;
a *virtual* cursor integrates the macOS-accelerated deltas, and the guest's cursor is **composited
into a `CALayer`** rather than worn by the pointer. While captured, the hidden host pointer also
*wears the transparent blank* whatever shape the guest sends (`input::WearState`) — AppKit can
unhide the pointer behind our hide refcount (observed on arrangement-driven window
reconfiguration), and a stray unhide must show nothing, not a live guest shape. Guest shapes
arriving mid-capture are stored and re-worn on release. **The wear is advisory and is therefore
checked, not assumed**: `[NSCursor set]` sets the application's current cursor and AppKit resets it
from its own cursor rects whenever it handles a mouse-moved — which, while the tap is consuming
motion, is a reset no event comes back for us to answer, so the blank was being stripped and
staying stripped (a second, frozen pointer on top of the guest's). `blank_cursor()` is memoised to
one instance so `HostCursor::verify_captured` can compare the real cursor against it by identity
every tick, report once per episode and re-wear.

**The captured cursor follows the guest.** Its position is a running value **in the device
range** (`capture_range`, `0..=ABS_MAX`), continuous for the whole session: each host delta is
scaled by a per-slot gain and clamped to the guest's **desktop** — which is a union of
rectangles and **never its bounding box**, the thing the range actually covers. Any vertical
offset or height mismatch leaves corners of that box on no monitor at all, and a position there
is over no output: the cursor plane is per-scanout, so nothing draws it and the cursor is simply
gone. `arrangement::Desktop::confine` holds the step on the desktop — a candidate that lands on
a monitor is taken as it is (that is how a seam is crossed), and one that lands nowhere is
clamped against the rect the *previous position* occupied, never the capture slot's, which the
echo leaves a step behind after a crossing. Every clamp is then at a wall by construction, so the
clamped-off motion is honest edge pressure and needs no filter (`fit::range_step`). The gain
comes from the same geometry — the slot's share of the range over its share of the window
(`fit::range_gain_of_share`) — falling back to a row-of-scanouts estimate (`fit::range_gain`)
only where the guest has reported no layout. The guest crosses
its own seams by itself, and its cursor echo (`window/echo.rs` — the scanout id every
`MOVE_CURSOR` carries, plus the plane origin and hotspot) names the slot and pixel it is on;
the render tick re-bases `capture_slot`/`capture_pos` to that slot's fit from the echo pixel
(`InputState::follow_guest_echo`, `fit::fit_point_of_pixel`) and logs every crossing
(`display: the guest's cursor crossed to slot N … (was slot M)`, info). The fit-space
`capture_pos` is only an *estimate* kept for drawing, the grab's edge press and the release
warp target; it is re-synced from every fresh echo. Nothing in this path needs the guest's
logical scale or its arrangement: a wrong gain moves the cursor a little faster or slower on
that display, it cannot mis-place it, because the guest decides which display a range value
lands on. (The retired desktop-space cursor guessed exactly that: it confined the cursor to a
union of monitor rects the host laid out itself, and whenever the guess and the guest disagreed
the sprite followed the guest while every trigger — press, park, release, pressure — followed
the guess, on the wrong display.) Consequences that keep catching people:

- A composited cursor is drawn *into a window*, so it can only ever show the display that window is
  showing. **Every window needs its own cursor layer**, and each draws its own slot. No host-side
  slot selection is correct here: the guest enables its hardware cursor plane on exactly one CRTC
  and hides it on the others, so per-slot drawing makes exactly one window draw. Asking where the
  *host* pointer is answers a different question, and in capture mode it is frozen anyway.
- The notch strip is a second window over the same layer bounds, so it needs its own copy of the
  cursor too, or the pointer vanishes on entering the housing band.
- **The two presentations can disagree about which slot has the plane, and a captured pointer then
  has nothing drawn under it.** The composited layer hides on its own slot's `visible == false`
  while the worn shape takes whichever slot has a plane, so a stale per-slot flag is invisible
  until someone is captured on that display. `cursor::undrawn_fault` is the standing check: the
  tick asks the window showing the capture slot for a `LayerVerdict`, and a non-drawing verdict
  that persists past `CURSOR_FAULT_SETTLE` while some slot *does* claim a cursor is one `warn`
  with the whole per-slot state — scanout, visibility, image id, size, position, and each slot's
  `CursorLog`, the last four writes that could have hidden it, with the timestamps. Everything
  shorter than the settle is a transient and passes in silence; it never panics.
- Capture runs through a session-level `CGEventTap` (needs Accessibility) *and* the local
  `NSEvent` monitor. **The two must map identically** — both call the one
  `InputState::captured_step_and_emit`; any new mapping must be shared rather than duplicated.
- `capture_pos` is a point in the view space of the window `capture_slot` names, and **nothing
  else, ever**; every write sets both in the same breath. A point in one window's space read
  against another window's geometry was the fault class behind every multi-display capture bug.
  `capture_range` is the position the guest actually receives; `capture_pos` is derived from
  the guest's echo, never the other way round.

## 5. The grab, and the edge that releases it

`docs/design/fullscreen-pointer-grab.md` is the full story of the release barrier. The
load-bearing parts:

- The grab exists so `Cmd-Tab` and friends reach the guest, and so leaving the VM's area is a
  deliberate act rather than an accident. **It spans every covered panel**: the regrab is judged
  in whichever guest window the free pointer is over, and the park lands in that window (the
  zero-length-warp guarantee holds per window, since `PARK_INSET = REGRAB_MARGIN` in each).
  **A click on guest content takes the grab at once** — the explicit ask, refused by nothing
  (not the margin, not the dwell, not an earlier explicit release); the press is delivered
  through the captured path and never reaches AppKit. **What a click landed on is the window
  server's answer, not geometry's**: a fullscreen guest window covers its whole panel, so the
  revealed menu bar, an open menu hanging over the guest's picture and any other app's panel are
  all *inside* the fit while being what the user is actually clicking
  (`InputState::guest_is_topmost_at`, `+[NSWindow windowNumberAtPoint:…]`; our own notch overlay
  is chrome and does not count). The hit test is about the **window**, the fit test about the
  **picture**, and the click path needs all three outcomes: not our window is the user leaving for
  macOS (stand down, so the walk back toward the guest does not re-take the pointer mid-menu); our
  window on the picture is the ask; our window off the picture — the letterbox, the band above a
  secondary's content — is *neither*, and reading it as a departure silently killed the re-grab
  and read as "clicking stopped working". **Gaining screen takes the grab too**
  (`grab_policy::gained_screen`, polled from the tick): going fullscreen, and a panel joining a
  session that is already fullscreen ("Use Other Screens When Fullscreen", a display plugged in).
  Neither reaches the tap, and the second happens while the user is still inside a macOS menu whose
  clicks have just stood the grab down. The silent re-take after an edge release
  keeps its hysteresis (deep inside for the dwell, no button down).
- **Some macOS UI takes the click without covering anything, and the hit test cannot see it.**
  Two known: an **open menu** (the click that dismisses it is spent on the dismissal) and a live
  **screen-capture session** — the Cmd-Shift-4 crosshair and its relatives, which intercept at
  the event layer, so the window server still answers our own window for a point over the
  guest's picture. Both are their own inputs to the policy (`menu_open`, `capture_live`), and
  both mean the same thing: no grab, and no departure either, so the *next* click is still the
  ordinary ask. The screenshot case is the sharper one — the grab would consume the drag and the
  mouse-up the selection needs, and take the keyboard with them, so Esc could not cancel what the
  click started. The session is found by bundle id (`com.apple.screencaptureui`) as a cheap
  filter and confirmed by an on-screen window it owns; the process alone is not the session, it
  outlives it. Both questions are deferred like the hit test — they are round trips, spent only
  once the policy has a reason.
- **Taking the pointer settles the chrome ask** (`toggle_capture_to` → `reveal_moot`, so every
  route in obeys it). A held grab and a granted reveal are mutually exclusive: the reveal exists
  so the pointer can reach the menu bar and the window's controls, and a captured pointer is
  hidden and pinned and can reach neither. The release direction already assumed the pairing —
  `grant_chrome` is the captured edge-push that frees the pointer *and* asks for the chrome — and
  without this half, enabling multi-display from the Displays menu while revealed grabbed the
  pointer and left the reveal standing, with nothing owning its undo. Same reasoning as leaving
  fullscreen, which also moots: the ask is not refused, it has stopped meaning anything. An
  explicit release
  (Cmd-Ctrl-G, the chord) latches the policy out until that next click, or until the pointer
  leaves guest content — a key regain no longer clears it (on a fullscreen-everything Mac the
  pointer can never leave guest content and the window never loses key, so the latch once
  stuck for good: rig, 2026-08-21).
- **The park follows the guest's cursor onto its panel.** It is derived in the grab's window
  at the grab (`PARK_INSET` deep — a zero-length warp by construction, since `PARK_INSET =
  REGRAB_MARGIN`) and the tap re-pins to it, zero-length, on every event, aimed at the window
  it currently sits in (`park_slot`). macOS routes trackpad Space swipes (and every
  display-addressed gesture) to the display the HOST cursor is on, so when the guest's cursor
  crosses to another panel the park must go too: once captured motion has paused for
  `REPARK_QUIESCENCE` (150 ms) the tick warps it into that panel's window
  (`InputState::repark_if_quiescent` → `WarpBroker::repark`). A nonzero warp while captured
  injects its vector into the delta stream with no suppression gap, and the SIGN depends on the
  regime (both measured 2026-08-20: same-display NEGATED, `LIMINA_WARP_PROBE` 32/32;
  cross-display +W riding one of the next 1-2 events, 10/10) — so the re-park arms an
  injection detector (`warp::WarpSwallow`, pure `swallow_step`) that recognizes the vector
  when it arrives and subtracts it, and reports a conservation skip if it never does; the
  pause is what makes the injected event arrive pure enough to recognize. The other nonzero
  warp is the release's, taken with the mouse disassociated and the cursor hidden, where it
  cannot become motion.
- **Every warp goes through the broker** (`window/warp.rs`, Move C). The CG warp/associate
  externs are private to that module, so no other code *can* warp; each broker method performs
  its whole obligation bundle atomically — `engage`/`disengage` (the capture transition:
  associate, warp, hide/show, wear, close the suppression interval), `repin` (the tap's
  zero-length per-event park), `probe` (`LIMINA_WARP_PROBE`'s raw measurement, deliberately
  uncompensated). The one warp-adjacent check that stays outside
  is the park zero-warp check in `toggle_capture_to`: it is view-space geometry judged before
  the CG-global conversion, and the broker speaks CG global only.
- **The guest's echo is learned from, the host's warps are asserted.** We tell the guest where
  its pointer is, and the guest tells us back where it put its cursor plane
  (`cursormove`/`cursorhide`, one plane on exactly one CRTC — the plane's origin, pointer minus
  hotspot; `window/echo.rs` stores the pointer). The guest, not the host, decides which display
  a position lands on, so the echo is treated as the **source of truth, not a test**: 150 ms
  after the last position (and the last relative push, which legitimately moves the guest's
  pointer) the tick reads which slot shows the cursor (`echo::shown_slot`, preferring the sent
  slot when a sprite straddles a seam), remembers it, and logs every change —
  `display: the guest's cursor is on slot N … (was M); we sent slot S unit (u,v)` — which is
  how a stock guest with two displays shows us its own mapping instead of crashing. A readable
  disagreement (`echo::verdict`: plane on the sent slot but more than 3 px from the pixel;
  `echo::at_edge_verdict` at a grab release: the guest not at the pressed edge of the capture
  slot) is a `warn` with the full state, never a panic; a guest showing no cursor at all has
  hidden its own pointer and is not judged. The **host-side** checks stay fatal (a supervisor
  panic hook SIGKILLs the worker's process group, so there is never a headless ghost guest):
  every broker warp carries an `Aim` (stage, slot, the displays that slot's window covers —
  `hostdisplay::displays_under_window`, a *set*, because a straddling window covers two) and
  asserts the target is on one of them before warping and that the cursor reads back at the
  target afterwards (`CGWarpMouseCursorPosition` is synchronous and lands on the target floored
  to whole points, measured 2026-08-21; the window server's clamp into the display union is
  what a miss means); a captured state with no park point is a violation (the old silent
  fallback to the main display's centre was the wrong-screen fault itself); the release asserts
  the step's slot is the capture slot and, for the in-place chrome release, that the target
  lands on the capture window's displays. Every message dumps the full state (stage, slot,
  target, aimed/landed displays, the arrangement). Unreadable facts skip the check (a
  windowless view, no displays for a slot); only a readable disagreement fires.
- It is released by a **sustained edge press**, not by crossing a boundary — the predecessor design
  (let the cursor escape, then warp it back) is retired and cannot be tuned into shape, because the
  window server has already moved the cursor by the time a tap sees it. Every warp also opens a
  ~0.25 s local-events suppression window. A press exists where the cursor is pinned at the
  capture window's fit edge — which, since the captured cursor follows the guest, is an edge
  of the guest's *desktop*: at a seam the guest's cursor crosses and the re-base moves the
  estimate off the edge before any charge accumulates. The release predicate is "a host
  display is beyond this edge"; what is on it (a macOS workspace, or one of our own guest
  windows, which re-grabs after its dwell) does not matter.
- Policy lives in `grab_policy.rs` as pure functions over values, with the display arrangement
  passed in as a `releasable` predicate. Put decisions there, not in the tap: six rounds of dogfood
  bugs on this feature were all in the decisions and none in the geometry. Since the Move B
  landing that home also holds:
  - **`WindowFacts`** — one ownership snapshot per guest window (key, on-active-space,
    has-screen, fullscreen), assembled ONLY by `InputState::window_facts()` (primary entry
    first, always present; primary fullscreen folds in the extend-strip flag). Every ownership
    question — the tap, the tick — reads this snapshot; never ask AppKit at a decision site.
  - **The release predicates**: `key_loss_releases` (any capture, hard grabs included; judged
    against the primary) and `fullscreen_exit_releases` (the policy's grab only) — the tap
    executes them per event for latency, the tick backstops key loss for the tapless path.
    `must_drop_grab` judges the window OWNING the captured cursor's slot (`capture_owner`), not
    the primary: swiping the cursor's panel away releases; a Space change on a panel the cursor
    is not on does not. (No swipe can produce that second direction — a swipe always targets the
    cursor's own panel, where the host cursor parks — but Mission Control and per-display
    fullscreen transitions can.) The free path mirrors it in *policy*: `free_arming` judges the
    arming flags by the window UNDER the pointer (`key` stays the primary's). The "re-grab over
    a secondary never engages" report was the explicit-release latch (the trace's `latched=`
    field), closed by the click rule above.
  - **The chrome reveal**: `RevealState` (a `Copy` value; `ask ∈ {None, Some(owner)}`) stepped
    by pure `reveal_step`; `InputState::with_reveal` is the single writer and keeps the
    `reveal_chrome` mirror in sync.
- **Recorded gestures are fixtures.** The policy takes `now` as data, so a dogfood round's
  `[EDGE]`/`[GRAB]` trace replays headlessly with an asserted verdict
  (`window/grab_fixture.rs`); charge telescopes over print-time stamps, so verdicts pin to
  within an event or two and pure-function fields pin exactly. Diagnose grab reports by replay
  first, rig time second.
- **Motion clamped off at a desktop edge is forwarded to the *relative* device as pressure**, so
  mutter's own barriers (the GNOME hot corner) still fire — a pointer is driven by **two**
  devices at once, captured or not. Captured, the clamp is the desktop's own boundary, so what
  it eats is a wall by construction; uncaptured, it is the fit clamp filtered to the slot's
  outer edges (`arrangement::outer_edges_at` — from the guest's report; every edge when there
  is none). A window's edge at a seam is not a desktop edge, and pressure there makes the two
  devices fight — teleporting, an unreachable band, a pointer that feels "pushed back".
- **A seam is a property of the point, not of the side.** An edge is a seam exactly where a
  neighbour is on the other side of it *there*; on a ragged desktop one edge is a seam over the
  span its neighbour covers and a wall over the rest, and any per-side answer gets one half
  wrong. Asking instead whether the edge sits at a bounding-box coordinate calls every offset
  monitor's leading edges seams and silently drops the pressure they are owed — a wall the guest
  holds the pointer against that charges nothing.
- **A seam the hand cannot follow is an edge** (`window/seams.rs`, pure `Hold`). Crossing a seam
  is only right when the display on the other side is one the user is looking at, so a captured
  step is held at the capture slot's own share unless **both** questions agree: the slot the
  *range* leads to past that side is the same slot the adjacent *host panel* is showing, and that
  slot is covered — fullscreen, on its panel's active Space, on a screen. Coverage alone is not
  enough, because host and guest arrangements disagree routinely (nothing in the pointer path
  relies on the relay — the shares are fitted from the echo, `absfit.rs`): with host panels
  `A B C` and a guest ordering them `A C B`, asking about A's right side asks about B while the
  range leads to C. Held, the seam becomes an ordinary release barrier and the edge press hands
  the pointer out onto the panel beyond, which is what the user asked for; the eaten motion is
  charged to nobody, because the guest's desktop really does continue past a seam and forwarding
  it as pressure sets the two pointer devices fighting. Two silences are deliberately different:
  a side with **nothing beyond it in the range** is the desktop's own outer edge and is left to
  the step's existing pin, while a **live display with no share fitted yet** holds every side at
  once — it is somewhere, and we cannot say where. Every held bound sits a small margin inside
  the share on both ends, since a share's facing value is already the neighbour's column 0 —
  and the margin is several scanout pixels, not one, because the guest places its cursor in
  *logical* pixels: one scanout pixel was 0.8 of one on the 1.25-scaled BenQ and left the
  hotspot on the neighbour's first column. Where every panel shows a covered, agreeing window
  nothing is held at all, and tests pin that.
- **A cursor that vanishes at a monitor's true edge is not a mis-placed pointer.** The plane's
  hotspot sits on the last scanline and the bitmap is clipped, so how much of it survives depends
  on the cursor's size on that display. The echo is what tells the two apart; the cursor is drawn
  in full at a monitor's *top* edge, where the arrow points away from the boundary.

## 6. Fullscreen, the notch, and why there is more than one window shape

- macOS gives each display its own Space by default, so per-display fullscreen is viable — but a
  **native fullscreen Space can never draw beside the camera housing**. `notch = extend` is
  therefore delivered by a borderless overlay above the menu-bar window level, not by a different
  kind of fullscreen; the strip is a second window that must be handed a copy of everything the
  carrier draws, cursor included.
- A covering secondary is a **native fullscreen Space on its own panel** whenever "Displays
  have separate Spaces" is on (the macOS default) — Mission Control, the swipe and the
  fullscreen animation included, in **both** notch policies: under `notch = extend` on a
  housing panel, each secondary carries its **own strip overlay**, the primary's design per
  window — the Space is the carrier, the strip rides above it showing the rows beside the
  housing, the window's layer overshoots its safe-area view by the same inset, and the
  secondary's own native fullscreen feeds the same learned per-panel inset store the
  primary's does (so the seam is exact even on a panel the primary never fullscreened on).
  Every guest window and its strip — the primary's included — is registered in the per-slot
  input registry, so events decode against the event window's own layer frame (band clicks
  resolve to the strip's slot; the shifted strip layer makes the math identical to the
  carrier's). Chrome-ask is deliberately absent on secondaries (parity-neutral: the old cover
  had none either). One case keeps the borderless above-menu-bar **cover window**:
  separate Spaces off (one Space spans every display, so a native fullscreen anywhere
  blanks the other panels; the cover itself draws beside the housing there, no strip).
  Covering is presentation only: **capture semantics do not change** — the grab still
  engages with the VM's fullscreen and the captured pointer already spans every covered
  panel, whichever mechanism covers each.
- A cover-window secondary takes the **whole** panel — withholding the housing band from
  the *window* would only leave the menu bar drawn in it. The window owns every pixel; the **layer**
  is what stops below the housing under `avoid`.
- A secondary letterboxes by the **same rule as the primary**, one per-tick refit per window:
  dynamic fills the usable box (the guest is driven to its shape, and bars would flash on rounding
  disagreements), host and fixed aspect-fit the guest's mode into it, centered, with the window's
  black background as the bars. The usable box is the content view's bounds, less the housing band
  only for a borderless cover under `avoid` (a native Space's view is already the safe area). The
  refit compares against the LAYER and writes on drift — never against a cache of intent (§7) —
  and the layer's frame is exactly what input measures events against (`target_of`,
  `window_of_slot`), so the pixels and the pointer move together by construction. Guest-follow
  (window resizes to a modeset) is dynamic-mode behavior only, as it is for the primary.
- A secondary is `orderFront`, never `makeKey`: keyboard reaches the guest through the primary, and
  a secondary that took key focus would swallow every keystroke while still showing pixels. That
  makes a **tracking area mandatory** for it to see motion at all — `setAcceptsMouseMovedEvents` is
  necessary and not sufficient, because AppKit delivers `mouseMoved` to the key window.

## 6b. The modifier row

The Mac's bottom row is Ctrl / Option / Command and a PC's is Ctrl / Super / Alt, so **Option sits
where Super does and Command sits where Alt does**. limina's *modifier normalization* is exactly
that positional identification — Control stays Control, the Option position becomes Meta/Super, the
Command position becomes Alt — which is what puts Alt under the thumb where a Linux desktop wants
it. It is on by default (`[input] normalize_modifiers` in `vm.toml`, `--no-normalize-modifiers` to
opt out) and lives on the menu bar under **Input ▸ Modifier Normalization**, which **persists per
VM** in the machine-state file beside the display switches — the config is only the fallback for a
VM whose menu has never been touched. Turned off, the guest gets whatever macOS reports and nothing
is touched.

Because the rule is positional it has to be applied to the **physical** key, and that is the whole
difficulty: **macOS applies its own Modifier Keys remapping in the HID layer, before any
application sees the event** — keycode *and* flags (measured, `spikes/modifier-mapping/`). So on a
Mac with Control↔Command swapped in System Settings, an unconditional swap composed with the user's
and delivered their Control key to the guest as Alt. Normalization therefore reads that
configuration back (`crates/limina/src/hostmods.rs`, the ByHost global domain via `CFPreferences` —
`NSUserDefaults` cannot see it) and inverts it to recover the physical key before mapping.

- **Only six usages are ever inverted** (both Controls, Options and Commands). Caps Lock → Control
  is the commonest macOS remap in existence and its owner wants the Control; inverting it would
  hand the guest a Caps Lock instead. A pair reaching outside the six is skipped entirely.
- **The inversion is the only thing that lives in physical space.** `modifier_is_down`,
  `reconcile_modifiers` and the pressed-set all stay keyed on the keycode macOS delivered, whose
  flag bits agree with it; `macos_keycode_to_linux_remapped` is the single seam where that space
  becomes the guest's. Host-side chords (Cmd-Ctrl-G, the ungrab chord) also stay in macOS space —
  the host's own remapping should govern host-side chrome.
- **The mapping is per keyboard and an `NSEvent` does not say which keyboard typed.** Disagreeing
  keyboards warn once and invert nothing, which is exactly the old behaviour. One case stays
  undecidable: a PC keyboard whose owner swapped Cmd/Opt in macOS to restore the positional feel
  gets that undone again in the guest.
- **Flipping the switch mid-hold would wedge a modifier.** A press and its release are two separate
  mappings of the same keycode, so a flip between them presses one evdev code and releases another.
  `set_normalize` drains everything held through the *old* map first; the config is read once at
  startup for the same reason.

## 7. Traps, each of which has cost a day

- **A gate and its emitter must share one geometry.** Wherever they were computed separately, the
  gate eventually judged a point in the wrong window's space and dropped motion the emitter would
  have mapped correctly.
- **Fullscreen captures the pointer**, so "I tested it fullscreen" is not a test of the uncaptured
  path, and vice versa. Say which one a result is about.
- **A window address is a map key with a reused address.** A window left registered keeps claiming
  its slot and the next window AppKit allocates inherits it, routing a whole display's input to the
  wrong monitor. Deregistration belongs in `Drop`.
- **Identical results across many toggles mean the differential is not reaching the system.** Check
  that the change is live before concluding anything from "no change" — and check whether the
  configuration can even distinguish the two models. Two panels at the same scale make a
  logical-units model and a pixel-units model *proportionally identical*, so a rig like that cannot
  falsify either.
- **Synthetic events are a poor oracle here.** A cursor warp opens a suppression window, and posting
  synthetic events needs Accessibility for the posting process; both show up as "the fix does not
  work" against working code. For anything the user must perceive, ask them.
- **The guest is authoritative about the guest.** Which display its cursor is on, how its monitors
  are arranged, what scale it chose — all of it is the compositor's to decide and ours to be told.
  Every host-side inference of one of these has eventually been wrong.

## 8. Verification

- Boot the rig the normal way (`cargo xtask run --disk <clone>`; see `docs/graphics.md` §8) — the
  windowed venus desktop is the vehicle for all of this.
- `LIMINA_EDGE_TRACE=1` prints `[CURSOR]` (what the pointer wears, and where; the guest's
  `move`/`shape`/`hide` echoes per slot), `[ECHO]` (each settled comparison of the guest's
  echo with the last position we sent, with the miss in pixels) and `[GRABSTATE]`
  (captured / key / on-active-space / has-screen / app-active) on transitions. `[CAP]` lines are
  captured motion (the capture slot and the cursor's view point in its window) — **their presence
  tells you the grab is on**, which is the first thing to check when a pointer result surprises
  you.
  `LIMINA_EDGE_TRACE=1` also prints `[EDGE]` (where the free pointer is, every event), `[CLICK]`
  (every uncaptured click with the facts that decided it: in-fit, fullscreen+key, Space, whether
  the grab was taken), `[GRAB]` and `[HITTEST]` (the window server's own answer for the point).
- `LIMINA_POINTER_WIRE_TRACE=1` prints every event actually written to the guest's pointer
  device. **This is the end of the host's story** — what is here reached the guest, what is not
  never left. It settles "is this ours or the guest's" in one grep, and did: a stuck button read
  as a dead desktop until the trace showed seven `type=1 code=272 value=1` writes and not one
  `value=0`. Pair it with `evtest`/a raw read of the guest's `/dev/input/event*` to confirm the
  other end.
- `LIMINA_INPUT_TRACE=1` prints every keyboard/modifier decision, including the drift between the
  host's bitmask and our believed pressed-set.
- **Two diagnostics report at the default level**, so an ordinary dogfood log carries them with
  nothing turned on: a sweep starting while the pointer is captured (which names the slot and axis
  that went unlearned — see §5), and the host pointer losing the guest's cursor shape (edge-
  triggered, and it names what it was found wearing, because "an arrow" and "nothing" are
  different faults that look identical on screen).
- `LIMINA_WINDOW_CAPTURE` dumps real pixels; a composited cursor and a worn `NSCursor` are
  distinguishable there, which is how to tell the two presentation paths apart.
- Pure geometry and policy (`fit.rs`, `grab_policy.rs`, `arrangement.rs`, `displays.rs`) unit-test
  headless. Anything expressible there belongs there — that is the whole reason those modules have
  no AppKit in them.
