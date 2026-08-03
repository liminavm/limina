# Fullscreen pointer grab: hold the cursor, release it on a sustained edge press

Status: **shipped 2026-08-02**, after five rounds of dogfood that changed four of its rules. Replaces the mechanism in
`display-modes.md` §"Retired: edge resistance", which stays documented as history. Companion:
`spikes/edge-pressure/RESULTS.md` (rounds 1–3, the measurements this rests on).

## Why the design this replaces could not be tuned into shape

`EdgeResist` let the cursor escape and then warps it back. That is not an implementation detail
to be improved — the window server has already moved the cursor by the time a `CGEventTap` sees
the event, so "hold the pointer" can only ever mean "put it back". Three consequences, all of them
felt, none of them tunable:

- **Every hold is a visible flick.** The cursor crosses the boundary and returns, once per event.
  Round 3's dogfood word for it was "jumping around".
- **Each warp opens a 0.25 s local-events suppression interval** (`input::end_warp_suppression`
  exists solely to close it again), so resistance and pointer latency are coupled.
- **The unit is wrong.** Off/light/standard/firm are 0/50/100/200 *post-ballistics points*, a
  quantity no one can perceive, which is why no value ever felt right. Measured round 3: a flick
  arrives as one 100+ pt event while a deliberate push arrives as ten 10 pt events, so the same
  nominal distance is either free or a wall depending on how fast you moved.

There is also a correctness tail we keep re-opening: the escape guard has now been wrong in both
directions in one day (too permissive → the pointer was dragged off the other display; too strict
→ side resistance vanished entirely), because "is the pointer outside" is genuinely ambiguous at
the moment of crossing.

## The design

**In fullscreen, the pointer is grabbed.** Same machinery as Cmd-Ctrl-G capture, which already
ships: the tap consumes motion, a virtual cursor is integrated and driven into the guest's
absolute tablet, and edge overflow goes to the relative device as barrier pressure. The host
cursor never approaches a screen edge, so nothing needs warping back, no macOS reveal can fire,
and `KEEPOUT` stops being necessary.

**A sustained press against an edge releases it.** The same charge model as the `notch = extend`
chrome ask (`input::reveal_step`): time actually spent pushing, capped per event so stillness
cannot be banked, decaying after idle, with a small distance floor against jitter. When the charge
crosses the configured hold, the grab drops, and **one** warp places the host cursor just past
that edge in the direction of the press so the motion continues naturally. It stays released until
the pointer comes back inside the window.

**The presets become durations**, which is the whole point:

| preset | hold | meaning |
| --- | --- | --- |
| Off | — | never grab; today's free pointer, unchanged |
| Light | 0.15 s | a brief lean gets you out |
| Standard | 0.30 s | deliberate |
| Firm | 0.60 s | you meant it |

Measured basis (round 2, chrome ask): an incidental corner push peaks at 0.02 s of charge, a
deliberate lean reaches 1.0 s, and 0.25 s sat comfortably between with 50x separation. A corner
*tap* therefore cannot release the grab at any of these settings — which is what makes the guest's
own corner and edge UI usable, including the top-right hot corner that today's design cannot
reach at all.

**The top edge is one gesture** (user-decided): a top-edge press releases the grab *and* puts the
`extend` overlay down, because reaching the menu bar is the reason to go up there. The separate
chrome-ask threshold folds into this.

**Fullscreen only** (user-decided). A windowed VM keeps an ordinary pointer: the window has
visible edges and trapping the pointer inside them would be surprising.

## The three ways this could feel worse, and what they require

- **Mid-drag release must not fire.** Dragging a guest window against an edge would otherwise
  ungrab and drop it on the other display. Suppress the release while any button is down; a press
  only counts once it starts after button-up.
- **The multi-display tax is real.** Grabbed means every glance-and-click on the other display
  costs a deliberate press. `Off` must be exactly today's behavior, and `Light` must be genuinely
  light.
- **Losing key must ungrab immediately**, or a background VM holds a pointer it has no claim on.
  An earlier draft of this document said the existing capture path already handled this. **It did
  not** — nothing releases capture on resign-key; the tap computes `is_key` (`capture_tap.rs:219`)
  and uses it only to gate *grabbing* (`:245`), while the captured branch (`:334-418`) consumes
  every session mouse event regardless of key state. Today that is nearly unreachable because a
  captured session eats Cmd-Tab and all clicks, so only a programmatically-raised window can take
  key. Always-on grab makes it routine — the close-policy Ask dialog (`mod.rs:1858`), a system
  alert, the Accessibility prompt — and the result is an unclickable dialog with the cursor hidden
  (`CURSOR_HIDDEN`, `input.rs:168`). **This is new code**, and it belongs in the tap next to the
  `is_key` it already computes, not on the 60 Hz timer.

## Four more hazards, from review

- **Re-entry oscillation is the likeliest way this ships worse than today.** "Released until the
  pointer re-enters the window" is geometrically impossible on a single display: fullscreen *is*
  the screen, the window server pins the released cursor to the top row, and that row is still
  window territory (the same fact documented at `fit.rs:499-510`). A cursor warped "just past" an
  edge sits 0-2 pt from re-entry, so the first inward jitter re-grabs, warps to centre and hides
  the pointer. Guard: a **re-grab hysteresis predicate** — pure, in `fit.rs`, tested headless
  before any wiring — requiring the pointer to be a real margin inside the content for a short
  dwell, with no button down.
- **`Off` must keep the uncaptured chrome ask.** With no grab there is no edge-press gesture, so
  folding the ask into the release would leave an `Off` user under `notch = extend` with no way to
  reach the menu bar at all. That is the *fourth* time this gesture would have been lost by
  attaching it to something that isn't always there (`fit::edge_duties` documents the first
  three). The captured release and the uncaptured ask are two policy sites over one shared pure
  charge core, and both get tests.
- **Two owners of `captured`.** A level-triggered policy ("fullscreen and key ⇒ grabbed") makes
  Cmd-Ctrl-G and the Ctrl-Opt ungrab chord no-ops in fullscreen, because the next tick re-grabs.
  Needs an explicit released-by-the-user latch, analogous to `soft_muted` (`capture_tap.rs:163`).
- **Never auto-grab without the tap.** Through the leaky local-monitor fallback, grabbing is
  strictly worse than today's resistance (clicks escape to other displays), and it would grab
  right past the Accessibility prompt that the explicit toggle deliberately keeps clickable
  (`mod.rs:2004-2021`). Gate the feature on `capture_tap::installed()`, and do **not** let
  entering fullscreen raise the TCC prompt — that stays an explicit Cmd-Ctrl-G affordance.

## Two quality problems the grab would promote to defaults

- **Captured scroll bypasses `ScrollAxis`** (`capture_tap.rs:401-414`): integer line fields and
  legacy detents, no precise deltas, no v120, no momentum. Acceptable in a short-lived mouselook
  mode; unacceptable as the default fullscreen trackpad scrolling. Fix before step 1 lands.
- **Capture release never ends the warp suppression.** `apply_capture_cursor`'s OFF branch
  (`input.rs:87-99`) re-associates and then warps, without calling `end_warp_suppression`
  (`input.rs:125`), so every release today eats up to 0.25 s of motion. Step 3's release warp
  inherits it and would silently break "motion continues naturally".
- Also unit-adjacent: the Control Center's `Custom (N pt)` path (`center/controller.rs:1545`) is a
  third home for the points unit that the migration must convert, not just `vm.toml`.

## Order of work — as built

The original 1→5 sequencing shipped the felt regressions first and the safety mechanisms last, and
its step 1 left the tree in a dangerous state — auto-grab with no release, escapable only by the
chord. Anyone booting an intermediate commit (dogfood, a bisect) would hit a trapped pointer. The
revised order below is what was built.

1. **Preset type + migration** — `2d90380`. Pure, unit-tested, no behavior change.
2. **The re-grab hysteresis predicate** as pure geometry in `fit.rs` — `2d90380`, alongside
   `pressed_edge` / `release_point`.
3. **The charge core + release mechanics** — `fc652b9`. `fit::Charge` extracted from the chrome
   ask so both policies share one accumulator, plus the `end_warp_suppression` fix.
4. **Captured-scroll quality fix** — `8aa72cf`, before the grab could make it the default.
5. **Wire the policy** — `16438f5`.
6. **Delete `EdgeResist`** — `16438f5`, the *same* commit as step 5, which the plan did not
   anticipate. They cannot be separated: with both alive, `edge-resistance` is read as seconds by
   the grab and as points by resistance at the same time, so a 0.3 pt threshold warps the pointer
   back at every edge — including the one it was just released at. A "reviewable" intermediate
   commit that is behaviorally broken is not reviewable.

## Two policy calls made during implementation

Both are refinements the plan did not settle; the second was got wrong first and corrected on
review.

- **Corners are never a press** (`pressed_edge` answers `None` inside `CORNER_ZONE`). Not in the
  plan above, which left it as an open question about single-axis presses. Leaning into the
  top-left corner is how the GNOME overview is opened; the clamped overflow keeps charging that
  barrier throughout, so releasing there would hand over the other display at the exact moment the
  user asked for the overview. This is the same conclusion the retired resistance model reached
  from a different direction, which is some evidence it is right.
- **An explicit Cmd-Ctrl-G grab is NOT the policy grab, in any mode.** It gets no edge release and
  is not ended by leaving fullscreen; Cmd-Ctrl-G and the Ctrl-Opt chord remain its only ways out.
  (I built the opposite first, reasoning that one grab should not behave two ways. Rejected on
  review: the two are different tools. The explicit grab is what you reach for when the pointer
  must not leave the VM *for any reason* — which is also why it is not the default — and giving it
  an edge release makes it the policy grab exactly where the distinction matters, leaving no way to
  ask for an unconditional hold at all.)

## What this deletes

`EdgeResist`'s warp-per-event, `KEEPOUT`, `SIDE_FACTOR`, the corner arm, the accumulator drains,
the re-arm rule, and the escape ambiguity — the entire distance model. The charge model in
`input.rs` becomes the single owner of "pressing at an edge", which is the invariant the last
three bugs in this area all violated in different ways.

## What five rounds of dogfood changed

Everything in this section was wrong in the design or in the first implementation, and none of it
was findable without hardware and a second display. The `[GRAB]` / `[EDGE]` / `[CAP]` trace
(`LIMINA_EDGE_TRACE=1`) is what turned each one from a theory into a fact; every time I reasoned
instead of reading it, I got it wrong.

- **The park point was the bug behind "the cursor teleports".** Capture has always parked the
  hidden host cursor at `main_display_center()`. The trace logged release targets in CG global
  coordinates and so incidentally mapped the test Mac: the VM's display sat at x ∈ [-1512, 0],
  y ∈ [879, 1861] — left of *and below* the main display. So the park was **on the other screen**,
  the grab warp was 1400-2400 points long, and a warp posts a motion event carrying its whole
  vector as the delta, which the captured path integrated as guest motion. Now the cursor parks
  where it already is, pulled 64 pt inside the content (`fit::park_point`), so the warp is
  zero-length. Largest captured delta went from 2400 to 107. **Two earlier attempts to detect and
  subtract the bogus delta were symptom treatment**, and the trace killed the second by showing 5
  of 13 grabs had no matching event at all.
- **Release re-associated the mouse before warping**, so the hardware was live while the cursor
  still sat at the park — on the other display — and motion in flight was applied from there. This
  is why a *left*-leaning push could land on a screen to the right, which no momentum story
  explains. Warp first, then associate.
- **A view point on the content's boundary can convert to a pixel one past the display.** The
  bottom row of a 982-point window at y ∈ [879, 1861] is y = 1861; the display's last row is 1860.
  `CGWarpMouseCursorPosition` clamps rather than failing, onto whatever screen is nearest. An
  in-place release now lands `RELEASE_INSET` inside the content.
- **Release only where there is somewhere to go.** "Just past the edge" is meaningless at an edge
  with no neighbour, and freeing the pointer there hands it to the window server still travelling.
  Checked per press against the actual arrangement, so a neighbour spanning part of an edge works
  along its whole length.
- **Cmd-Ctrl-G toggles the HARD grab, not `captured`.** Toggling `captured` stopped being coherent
  the moment the policy existed: in fullscreen the policy holds the pointer nearly always, so the
  combo always found it grabbed and always released, and by the time a second press arrived the
  policy had retaken it. The hard grab was simply unreachable. Now: soft or policy-held → hard
  grab (a promotion in place, no capture transition); hard → release; Ctrl-Opt always releases.
  User's design.
- **The `user_released` latch clears when the pointer leaves the guest**, not only on a focus
  regain. On a fullscreen VM there is nothing else on that display to click, so the regain edge
  never came and one Cmd-Ctrl-G disabled the grab for good. It must still survive while the
  pointer stays inside, or the re-grab undoes the release a quarter second later.
- **The bottom edge is ordinary.** It briefly had a release-in-place special case, to reach what
  looked like a dash at the bottom. It was the macOS Dock, and it only appeared *while the cursor
  was teleporting* — a special case built to serve a symptom, which passed my own review by
  looking like a reasonable accommodation.

## Still open

- Does `Firm` at 0.6 s feel like control or like a stuck pointer? Only `Standard` has had real use.
- Is `REGRAB_DWELL` (250 ms) right in both directions — long enough never to re-grab during a
  glance at the other display, short enough not to feel dead on return?
- Does the multi-display tax bite over a full day? Every glance-and-click on the other display
  costs a deliberate press.
- `RELEASE_OFFSET` (8 pt past the edge) was chosen to clear `REGRAB_MARGIN`, not from any
  measurement of where a released pointer should appear. It reads fine; it is not tuned.
