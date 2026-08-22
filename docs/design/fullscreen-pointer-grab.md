# Fullscreen pointer grab: hold the cursor, release it on a sustained edge press

Status: **shipped 2026-08-02**, after five rounds of dogfood that changed four of its rules. Replaces the mechanism in
`display-modes.md` §"Retired: edge resistance", which stays documented as history. Companion:
`spikes/edge-pressure/RESULTS.md` (rounds 1–3, the measurements this rests on).

**Multi-display:** the captured cursor follows the guest — the window it is judged in is the
one showing the slot the guest's cursor echo names (the grab's window at first, re-based on
every crossing), and everything in this document is evaluated in **that window**: the charge
model, the release barrier and its constants, the park/regrab coupling, the policy purity. Where
it says "the content" or "the fit", read "the current capture window's content". A seam to a
neighbouring guest window is not an edge: the guest's cursor crosses it on its own and the
capture window switches with it; only the guest desktop's outer edges pin, press and release
(`docs/input-and-windows.md` §4-5).

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
  where it already is, pulled `PARK_INSET` inside the content (`fit::park_point`), so the warp is
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
- **The `user_released` latch clears on a click on guest content** (which also takes the grab
  at once — the click is the explicit ask) or when the pointer leaves the guest; never on a
  focus regain. On a fullscreen VM there is nothing else on that display to click, so the
  regain edge never came and one Cmd-Ctrl-G disabled the grab for good; on a
  fullscreen-everything Mac the pointer cannot leave guest content either, which is how the
  leave-based clear stuck the same way (rig, 2026-08-21). The latch must still survive while
  the pointer merely rests inside, or the re-grab undoes the release a quarter second later.
- **The bottom edge is ordinary.** It briefly had a release-in-place special case, to reach what
  looked like a dash at the bottom. It was the macOS Dock, and it only appeared *while the cursor
  was teleporting* — a special case built to serve a symptom, which passed my own review by
  looking like a reasonable accommodation.

## Round six: the residue of the park bug (2026-08-03)

Dogfood, after the fix above shipped: coming back fast from the external display, clicking the
guest's top-right system menu, then moving toward an item — the pointer skips once. Only the first
time, which points at the automatic re-grab rather than at anything continuous.

Both remaining sources were in the grab's *seed*, and both are the same fault as the original park
bug at a smaller scale — a warp whose length becomes guest motion:

- **`PARK_INSET` (64) was larger than `REGRAB_MARGIN` (40)**, so a pointer the policy was willing to
  re-grab could still be pulled up to 24 pt. Those two numbers were never independent: the re-grab
  only fires on a pointer already `REGRAB_MARGIN` clear of every edge, so `PARK_INSET =
  REGRAB_MARGIN` makes every policy grab warp-free *by construction*, and a test asserts the
  coupling instead of trusting the constants to stay in that order.
- **The seed was remembered, not observed.** `toggle_capture` derived both the guest's virtual
  cursor and the park from `capture_pos`, which is fed by the window's motion handling — and the
  tap that decides the re-grab runs *ahead* of it, so the seed was a whole event stale (a long way
  at speed), and stale by an entire excursion after a trip to another display, where the content
  gate stops updating it at all. It now asks the window server where the pointer actually is
  (`NSEvent::mouseLocation`) whenever that is over the content. Off the content — a keyboard grab
  taken from another display — the remembered position is still the better answer: clamping the
  live one would drag the guest cursor to the nearest edge, and the warp is long either way.

The general lesson, third time in this file: **any warp taken while captured is guest motion.** The
only safe warp is a zero-length one, and the way to get that is to make the geometry guarantee it
rather than to measure the injection and subtract it.

### And what the fix uncovered: the gate disagreed with its emitter

With the seed honest, dogfood reported the teleport gone but a *flicker* — the pointer flashing
toward the top, then continuing along its path. The trace said the grab was innocent: `[CAP]` deltas
right after it were ordinary motion, but `[GRAB]` showed `pos` (the remembered virtual cursor)
sitting **exactly** where the tap had seen the pointer 350 ms earlier. The guest cursor had been
*frozen* for a third of a second while the tap saw every event; the grab re-seeded from the truth
and the cursor snapped, which is the flash.

The cause was `pointer_inside`, the gate deciding whether uncaptured motion reaches the guest at
all. It did its own `view.convertPoint_fromView(event.locationInWindow(), None)` — precisely the
combination `event_point_in_view` exists to correct. Under `notch = extend` the guest view lives in
the overlay while events are still delivered to the carrier, so the two spaces differ by the
carrier's height: measured **949 pt** (`[MON]` trace, corrected y = 959.5 against naive y = 10.5).
The emitter was fixed when that trap was first found; the gate never was, so the gate judged a point
in the wrong window's space and dropped motion the emitter would have mapped correctly. One session
of ordinary use: **24 motion events dropped**, plus 92 more where the position was wrong by that
offset without flipping the verdict.

Two things worth keeping from this:

- **A gate and its emitter must share one conversion.** Two owners of "where is the pointer" is the
  same fault as two owners of "pressing at an edge" (see the chrome ask above), and it fails the
  same way: silently, intermittently, and only in the configuration nobody tests.
- **The freeze was a second, independent cause of "the cursor teleports".** Before the seed fix the
  grab inherited the *same* stale position, so the frozen cursor and the grab agreed with each other
  and motion simply continued from the wrong place. Fixing one made the other visible. When a
  symptom has been chased this many times, expect more than one producer.

## Round seven: the top and the sides are not one gesture (2026-08-03)

Dogfood, on the same day: *"the threshold for the push at the top edge to bring the chrome feels
great — I can trigger it whenever I want and haven't done it accidentally so far; but the sides feel
a bit too hard for the standard."*

That is not a badly-chosen constant, it is one constant doing two jobs. Pushing **up** asks for the
macOS chrome: the target is visible, the user is aiming at it, and a firm hold is what keeps it from
happening by accident. Pushing **sideways** means "let me out onto the other display" during
ordinary travel — it happens mid-motion, with nothing on screen to aim at, and usually as two or
three shoves with a hand reset between them rather than one steady lean.

So `fit::edge_timing(hold, edge, tuning)` states the asymmetry once:

- the **top** keeps the configured hold exactly, and the baseline `CHARGE_DECAY` (0.4 s) — that is
  the number dogfood says is right, and nothing should drift it;
- the **sides** (including the bottom, ordinary since the day before) ask `SIDE_HOLD_FACTOR` of it
  (0.6 → 0.18 s at `Standard`) and get `SIDE_DECAY` (0.9 s) of grace between strokes, so a hand
  reset continues the gesture instead of ending it.

`Charge::push` takes the decay per call rather than reading a constant, which is what lets one
accumulator serve both feels. The reduction must buy forgiveness and not accidents, so a test
asserts the *scaled* hold still rejects the measured three-event corner flick at every preset, and
another asserts stillness is still unbankable however long the grace period is (`CHARGE_TICK_CAP`
does that job, not the decay).

`LIMINA_SIDE_HOLD_FACTOR` and `LIMINA_SIDE_DECAY` dial both numbers for a session, so the next
round of this question costs a relaunch instead of a rebuild. They are tuning aids, not interface:
a value that proves better becomes the default.

## How this gets pinned (agreed 2026-08-03, not built yet)

Six rounds of bugs, and **not one of them was in the geometry** — `fit` has been tested from the
start and has been right. They were all in the policy, which is the one part with no test, because
it is welded to `CGEventTap` and `NSView`. Adjusting a constant today means re-dogfooding by hand.

A literal injected-input test cannot be a suite test: the policy only runs from a session tap, the
tap needs Accessibility, and that grant is **per-binary** — a test harness would need its own (see
`limina-tcc-adhoc-accessibility`). So the plan replays *samples*, not `CGEvent`s:

1. **Lift the policy out of the tap into a pure step function.** `capture_tap` mixes CG plumbing,
   policy, and effects; split so the middle is `step(&State, Sample) -> Vec<Action>` with
   `Sample { pos, delta, buttons, mods, fullscreen, key, now }` and
   `Action::{Grab{seed}, Release{to}, GrantChrome, Consume, Pass}`. Take the display arrangement as
   a **parameter** instead of calling `CGGetDisplaysWithPoint` inline, so a two-display layout is
   test data and the dead-edge rule is checkable headlessly.
2. **Replay recorded gestures as fixtures.** `LIMINA_EDGE_TRACE` already emits everything a `Sample`
   needs. Each dogfood round becomes a recorded trace with an asserted verdict — the re-entry
   gesture, the corner lean, the top-edge chrome ask, the press at a dead edge. Then changing a
   constant shows you *which* gesture changed its mind.
3. **The invariant that would have caught all three park bugs at once: the guest cursor moves only
   by host motion.** Every one of them was a warp injecting its own vector. Assert
   `Δguest_abs == Σ host deltas` per event window, excluding the deliberate discontinuities (the
   release warp, the content clamp). Cheap enough to ship as a runtime check behind an env var, so
   dogfood *reports* a skip instead of the user having to notice one.

Order matters: (1) is a refactor of the file that changed all week, so it lands after a dogfood
validation, never bundled with a behaviour fix. (2) and (3) are small once (1) exists.

## Round eight: key status is not "on screen" (2026-08-08)

Two dogfood reports, one root cause. The hard grab would stick through a Mission Control gesture
(Ctrl-Up or three fingers up), and after an unplug/replug that relocates our Space the pointer would
work for a moment, then freeze and vanish "after a little bit of idle".

The trace (`LIMINA_EDGE_TRACE`, `[GRABSTATE]` — captured / key / on-active-space / has-screen /
app-active, on transitions) settled it in one run and inverted the fix I was about to write:

```
[GRABSTATE] t=322393.2 captured=false key=true on_active_space=false …
[GRABSTATE] t=324494.8 captured=true  key=true on_active_space=false …
```

The grab was **taken two seconds after our Space had already left**. Through Mission Control, `key`,
`app_active` and `has_screen` all stay true — *only* `on_active_space` moves. So `fullscreen_and_key`,
the test for "is this pointer ours to think about", said yes about a window the user could not see:
the pointer came to rest somewhere over where our fullscreen window lives, served the re-grab dwell
(hence "after a little bit of idle" — that is `REGRAB_DWELL`), and capture parked and hid it on a
Space limina was not on. There was no way back short of finding limina again.

Both directions are needed and the fix is symmetric:

- `Free::space_visible` joins the ownership test in `free_step`, so the grab is never *taken* from a
  Space we are not on — and the `reset_gesture` on the way out means the dwell spent there is not
  banked for our return.
- `must_drop_grab` releases a grab already *held* when the Space (or the screen) goes away, polled
  from the window tick. Without it the pointer stays parked through Mission Control; without the
  first, the tap re-grabs on the next dwell and the two fight at 60 Hz.

**The same trap had a second instance one input over**, found immediately after: the *soft keyboard
grab* also engaged on key status alone, so with the pointer correctly released the guest went on
eating keystrokes aimed at Mission Control. `soft_keyboard_engaged` now takes `space_visible` too.
And the held-key flush moved to the Space-leave *edge*, unconditionally: the gesture that takes the
Space away is itself made of keys, so Ctrl-Up reaches the guest as Ctrl-down while the soft grab is
still engaged, and its key-up then goes to macOS — leaving Ctrl stuck down in the guest for good.

The generalisable part: **`isKeyWindow` survives a Space switch, and every ownership question in
this feature was asking it.** "Is this window focused" and "is this window on screen" are different
questions, and the input layer needs the second one wherever it takes something away from the user.

## Still open

- Does `Firm` at 0.6 s feel like control or like a stuck pointer? Only `Standard` has had real use.
- Is `REGRAB_DWELL` (250 ms) right in both directions — long enough never to re-grab during a
  glance at the other display, short enough not to feel dead on return?
- Does the multi-display tax bite over a full day? Every glance-and-click on the other display
  costs a deliberate press.
- `RELEASE_OFFSET` (8 pt past the edge) was chosen to clear `REGRAB_MARGIN`, not from any
  measurement of where a released pointer should appear. It reads fine; it is not tuned.
- An **explicit** grab (Cmd-Ctrl-G) taken with the pointer within `PARK_INSET` of an edge, or off
  the content entirely, still warps — and so still injects that distance as guest motion. The
  policy re-grab cannot reach either state, so this is rare and user-initiated; the honest fix is a
  park that does not have to be inside our own content, which nothing needs yet.
