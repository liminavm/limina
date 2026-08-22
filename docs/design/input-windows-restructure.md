# Restructuring input & windows for many displays

Status: **landed** — all four moves done (decided 2026-08-20 from the post-arc structural
review; A/D/B landed 2026-08-20, C 2026-08-21). Companion: `docs/input-and-windows.md` — the map
has absorbed every move's shape and is the document to read first. This one is kept as the
rationale record while the booked follow-ups in `docs/hardening-backlog.md` §Display/window
(the free-regrab-over-secondary gap, the hidden-seam execution fault) still work this territory;
when those land it reduces to a short historical note or is deleted.

## The diagnosis

The multi-display arc's bug ledger is unambiguous. Of the arc's window-area commits, the two
dominant fault classes — **primary/secondary asymmetry** (8 commits as primary cause) and
**duplicated/stale state authorship** (12 involved) — are one class wearing two names: the primary
window was the de facto spec, encoded nowhere as a spec. Every mechanism perfected for one display
(grab, chrome ask, letterbox, notch strip, cursor presentation) had its authoritative
implementation inlined into `run()` in `window/mod.rs`, closed over THE window's bindings, and the
arc re-derived each one for `SecondaryWindow` by hand — a parity-porting sequence (`aceb9b7` →
`20f1a7c` → `2c101b4` → `d306d42` → `930ceff`), each step catching an asymmetry the previous one
left behind. Solved battles came back because the *solutions* lived in one window's code path, not
in a shared mechanism.

The secondary breeding ground: policy keeps accreting outside `grab_policy.rs`. The chrome-reveal
gesture (`input.rs::reveal_step`, ~155 untested lines with owner-slot switching), the
warp-injection detector (`WarpSwallow` — empirically tuned, regime-dependent sign, no tests), and
two of the three grab-release conditions hand-coded in the tap callback
(`capture_tap.rs:305-328`). `docs/design/fullscreen-pointer-grab.md` §"How this gets pinned"
already agreed the fix (pure `step(&State, Sample) -> Vec<Action>` + fixture replay + the
conservation invariant) and it was never built; multi-display has since multiplied exactly the
code it was meant to pin.

Every move below is a pattern that has already won in this codebase, extended to the sites still
hand-rolled: `ExtendOverlay` (one type, instantiated per window — the only window-stack piece with
zero parity bugs in the arc) proves A; `grab_policy`'s purity (six dogfood rounds of grab bugs,
none in the tested geometry) proves B; the notch arc's "compare against the state itself, never a
cache of intent" proves D; `repark_if_quiescent`'s warp+rewear+arm bundle is the seed of C.

**No move changes behavior.** Extend-notch, fullscreen-as-Spaces, and the auto grab/release feel
are untouched; these are changes to authorship and ownership, not semantics. Each move lands
suite-validated and is never bundled with a behavior fix.

## Move A — finish the GuestWindow inversion — **done**

**The primary is the first window, not a different kind of window.**

Landed, in two halves. The shared mechanism lives in `GuestWindow` (`window/guestwindow.rs`):
present/resolve/resurface-recovery/ack, the release drain, the letterbox rule
(`fit::refit_target` + `apply_fit`), THE band-inset rule (`strip_inset`), the inset learn, the
strip mirror/seed and the capture-cursor walk — used identically by every window. The collection
inversion lives in `window/windows.rs`: `GuestWindows` owns the primary (wearing a
`PrimaryDisplay` role value) plus one `SecondaryWindow` per other connected slot, and one
`apply()` per tick runs every window's walk in one shape — strip reconcile → refit → gen gate →
modeset follow → present — off one snapshot taken under one lock. "Primary" is a role value:
it carries only what exists once (the relaunch size, present/capture diagnostics); key/keyboard ownership, the menu, close policy,
lifecycle/park, `state.toml` persistence and the display control plane stay with `run()`.

This retires the dominant bug class: the next per-window feature is written once and every panel
has it; there is no "port to secondaries" step left to forget. Secondary placement persistence —
the next parity gap users would hit — falls out of the same shape instead of being another port.

Two asymmetries are kept deliberately, marked in place: the primary's gate order (`show_id`
before the modeset follow, where a secondary follows geometry first), and the primary's absence
from the `WINDOW_SLOTS`/`SLOT_WINDOWS` registry — registering it moved to Move D, below.

## Move B — one ownership snapshot, one pure policy engine — **done**

Landed in five suite-validated increments.

**The snapshot.** `WindowFacts` — slot, primary, key, on-active-space, has-screen, fullscreen —
is defined in `grab_policy` and assembled in exactly one place, `InputState::window_facts()`:
the primary entry first and always present (a windowless primary reports all facts false except
fullscreen, which folds in the extend-strip flag — `overlay_active` and the tap's `panel_fs`
turned out to be the same `Arc`, so the tap's own fullscreen read is gone), then one entry per
hosted secondary. Consumers: the tap callback, the tick, the captured stepper's visibility
filter, and `guest_surfaces` — every ownership question reads the same snapshot. `covering` is
deferred until something consumes it, and a secondary's `fullscreen` reports false until
secondary fullscreen exists.

**The owner-judged drop** — the move's ONE deliberate behavior delta, landed as its own commit:
`must_drop_grab(captured, owner)` judges the window owning the slot the captured cursor is on
(`capture_owner`), not the primary. Swiping the cursor's own panel to another Space releases the
grab; a Space change on the primary's panel while the cursor drives a different one no longer
falsely releases (a direction no swipe can produce — a swipe always targets the cursor's panel,
where the host cursor parks — but Mission Control and per-display fullscreen transitions can),
and a missing owner window drops. Single display is unchanged — the owner IS the primary there.
The rig pass surfaced the free-path mirror: `free_arming` (landed right after the move) judges
the `Free` sample's fullscreen/Space flags by the window under the pointer (key stays the
primary's) — necessary but not yet sufficient: the rig still shows no re-grab over a secondary
at all, booked in `docs/hardening-backlog.md` §Display/window for after the rework.

**The pure predicates.** `key_loss_releases` (releases ANY capture, hard grabs included; judged
against the primary, which owns key/keyboard routing) and `fullscreen_exit_releases` (only the
policy's own grab) live in `grab_policy`, unit-pinned. The tap stays the per-event executor —
that placement was deliberate, for latency — and the tick runs `key_loss_releases` as the
backstop for the tapless degraded path. Fullscreen-exit has no tick backstop: `GrabState.holding`
lives in the tap's context, and a stale hold is benign (`free_step` re-grabs by dwell).

**The reveal purified.** `RevealState` is a `Copy` value (charge, granted, last-pos, owner, ask;
invariant: `ask ∈ {None, Some(owner)}`) stepped by pure `reveal_step` in `grab_policy`, with
`reveal_grant` shared by the outright-grant and menubar paths and `at_panel_top` as the menubar
grant's targeting rule; the `REVEAL_*` constants moved with it, 11 replayed-branch unit tests pin
it. `InputState::with_reveal` is the single writer and syncs the `reveal_chrome` mirror on every
change — found defect 5's honest home, now resolved.

**Fixture replay.** `window/grab_fixture.rs` replays recorded `[EDGE]`/`[GRAB]` trace lines
through the pure policy (`now` is data; charge telescopes over print-time timestamps, so
verdicts replay to within an event or two and pure-function fields replay exactly). The first
fixture is the open hidden-seam trace, and it converted the pending diagnosis into a verdict:
the policy charged the left-edge press past its hold and RELEASED through the seam
(`x = fit.x − RELEASE_OFFSET`), then re-grabbed on return — the rig-felt "wobble, no way to
cross" happened after the verdict, in execution (warp/visibility). That is Move C's territory.

**Conservation.** What shipped is the expiry-time reporter: `WarpSwallow` sums every delta seen
while armed, and an arm that expires having absorbed the injected warp as guest motion logs a
`warn` conservation-skip line (always-on rather than behind an env var — a deviation from the
plan text above, taken because the cost is expiry-only and `warn` is dogfood's default level).
Move C's broker inherited this reporter unchanged, and it is the shipped form of the
conservation invariant: the only injections are warps, warps exist only at broker sites, and
every armed warp is now accounted for at expiry. The full per-event
`Δguest_abs == Σ host deltas` ledger was deliberately not built — it would add per-event cost to
catch only faults the reporter already names, and Move C was behavior-preserving.

## Move C — a warp broker — **done**

The single most-repeated fault here (five pre-arc instances, one in the arc — the 2W
double-crossing jump) is a warp or its echo becoming guest motion. Every warp site carried three
obligations by convention: close the ~0.25 s suppression window, re-assert the blank wear (the
ghost-cursor class), and arm/compensate the injected delta, whose sign is regime-dependent
(same-display negated, cross-display positive).

Landed as `window/warp.rs`: the `CGWarpMouseCursorPosition` /
`CGAssociateMouseAndMouseCursorPosition` externs are **private to the module**, so no other code
can warp — the ownership rule is a compile-time fact, not a convention. `WarpBroker` (a field of
`InputState`) performs each bundle atomically: `engage`/`disengage` absorbed
`apply_capture_cursor` and `end_warp_suppression` unchanged (both `associate(1)` calls kept, in
order — the suppression-close behavior was measured and is not simplifiable); `repin` is the
tap's zero-length per-event park; `repark` is the one legitimate nonzero warp (warp + re-wear +
detector arm, returning the armed vector so the slot-tagged log stays at the call site); `probe`
is `LIMINA_WARP_PROBE`'s raw measurement warp, deliberately uncompensated. `WarpSwallow`'s
stepping became the pure `swallow_step`, and the thresholds are unit-pinned in one place: the
half-|W| recognizer (boundary inclusive), the 3-event expiry, the conservation verdict at
expiry — including the two shapes that were only conventions before (a zero-vector arm can never
fire and expires silently; the recognized event never joins the conservation sum, its W content
being accounted for by the subtraction). The broker inherits Move B's conservation reporter.

One check deliberately stayed behind: the park zero-warp check in `toggle_capture_to` is
view-space geometry judged before the CG-global conversion (seed vs `PARK_INSET`), and the
broker speaks CG global only — dragging it in would trade the geometric guarantee for a
conversion round-trip.

## Move D — one geometry-sourcing rule for the gate — **done**

The primary's input gate trusted `fit_cell` — a cache of intent written per tick — while a
secondary's gate reads the live `CALayer` frame at event time; guarding on a cache of intent is
THE RULE the notch work paid for three times. Landed in two halves, both behavior-preserving:

- **The gate reads the layer.** `GuestWindow::fit()` reads the scanout layer's frame — the same
  read `window_of_slot` does for a secondary. `InputState` holds the primary core and answers
  `primary_fit()` from it; `fit_cell` is deleted (the render side keeps only a private
  trace-edge cell for the letterbox debug log, and the present path's host/fixed gate became
  `apply_fit` — compare against the layer, strictly better on AppKit drift).
- **The primary (and its strip) registers in `WINDOW_SLOTS`/`SLOT_WINDOWS`**
  (`PrimaryDisplay::register`, reconciled per tick because a panel handover moves the primary's
  slot; stale entries pruned by window identity). `target_of` decodes every guest window through
  the one registry path — `locationInWindow` against the event window's own layer frame, the
  primary flag computed from the slot — retiring the band-decode class where a primary strip
  event was converted across windows into the carrier's space. The primary-view fallback remains
  only for events from non-guest windows, where landing outside the fit and being dropped is the
  point. (`emit_motion`'s primary-only tail re-derives its own point and fit, so it never
  depended on the Target's coordinate space.)

The map's invariant — "the layer's frame is exactly what input measures events against, so the
pixels and the pointer move together by construction" — now holds for every window.

## Found defects, independent of the moves

Small and concrete; none blocks or is blocked by the restructure. Listed so they aren't lost, not
as an excuse to defer the moves:

1. `pointer_slot` freezes during capture (only `emit_motion_to` writes it), so release re-wears
   the pre-capture slot's cursor scale — one wrong-sized cursor frame when a capture session
   releases on a different-scale panel.
2. `TapCtx` held a copy of the virtual cursor cell — **resolved**: the cell is `InputState`'s
   alone.
3. `fit::capture_step` read like THE capture stepper while only `park_point` used it —
   **resolved**: it is the capture stepper's clamp again.
4. `park` is the lone CG-global field in a one-space-per-field struct; newtype or loud comment.
5. `reveal_chrome` is a bare global mirroring a per-slot fact — **resolved by Move B**:
   `with_reveal` is the one writer and syncs the mirror on every state change.
6. Secondary caches lack the primary's proactive release purge (bounded by `SURFACE_STORE_CAP`,
   so asymmetry, not leak); dissolves under Move A.
7. No secondary placement/fullscreen persistence; dissolves under Move A.

## Sequencing

**A → D → B → C — all landed.** A is the enabling inversion and is mechanical (the `aceb9b7`
extraction proved the seam). D was nearly free once A existed, and landed before B so the policy
engine consumes one trustworthy geometry source. B landed and did exactly that: the hidden-seam
defect was diagnosed by fixture replay rather than rig time (execution-side). C closed the arc as
a behavior-preserving landing. Per the grab doc's own rule: a refactor of a hot file lands after
a dogfood validation, never bundled with a behavior fix — each move was a separate,
suite-validated landing. Still open after the rework: the booked free-regrab-over-secondary gap
and the hidden-seam execution fault (both `docs/hardening-backlog.md` §Display/window) — the
broker gives their diagnosis one module to read.
