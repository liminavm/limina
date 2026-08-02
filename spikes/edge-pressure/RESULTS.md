# edge-pressure — why the GNOME hot corner would not fire through a limina window

**Verdict (2026-08-02): the guest was never getting the push, only the position.** Not a threshold
problem, which is what two rounds of tuning assumed.

## The control that killed the theory

`guest-corner-control.py` creates a uinput mouse *inside the guest* and drives it into the
top-left corner. The overview opened **during the parking phase**, before a single measured push:

```
overview before: b false
overview after parking: b true
```

mutter is innocent, its barrier is easy to satisfy, and the whole problem is host-side. This took
about two minutes and invalidated everything I had reasoned my way to from the outside.

## What the host trace then showed

`LIMINA_EDGE_TRACE=1` logs every resistance decision. 1823 real events from one dogfood session:

```
cur=(0.0,982.0) d=(-48.0,-21.0) free=false overflow=(-48.0,-21.0)   <- absorbed and forwarded
cur=(0.0,982.0) d=(-20.0,-10.0) free=false overflow=(-20.0,-10.0)
cur=(0.0,982.0) d=(-39.0,-20.0) free=false overflow=(-39.0,-20.0)
cur=(0.0,982.0) d=(-35.0,-19.0) free=false overflow=(-35.0,-19.0)
cur=(0.0,982.0) d=(-29.0,-16.0) free=true  overflow=(0.0,0.0)  revealed=true   <- let go
cur=(0.0,982.0) d=( -9.0, -6.0) free=true  overflow=(0.0,0.0)   <- and silent from here on,
cur=(0.0,982.0) d=(-16.0,-10.0) free=true  overflow=(0.0,0.0)      however long the user pushes
```

Three separate faults, none of them guessable:

1. **No forwarding at all without the capture tap.** Pressure was sent only from captured mode and
   from `capture_tap::resist_edges`. No Accessibility grant → no tap → no pressure → the hot
   corner is *unreachable*, permanently. A core guest interaction had quietly acquired a
   permission dependency. (`emit_motion` now forwards it too.)
2. **Breakthrough ends the pressure.** 142 px delivered, then zero forever. No burst size can
   serve a barrier that wants sustained motion, so the earlier "hold corners to 3× the threshold"
   fix was the right diagnosis at the wrong layer. Corners now never release.
3. **The guest's cursor lagged the clamp.** The local monitor is the only other thing driving the
   absolute device, and it never runs while the tap consumes — so the guest cursor sat tens of
   points short of the corner and spent the first part of the forwarded push travelling there.
   142 px measured ≈ 90 px of actual barrier pressure, against mutter's 100. Hence "it worked
   once and I can't do it again".

Also visible in the same trace: the chrome reveal firing on two events of `d=(0,-70)`, i.e. the
tap's reveal path was still distance-based after the monitor's had become a hold. Two owners of
one gesture; only one had been reworked.

## Round 2 (2026-08-02): tuning the chrome ask from a recording

The gesture that asks for the macOS chrome back was tuned the same way, and every constant that
had been guessed turned out to be guarding against the wrong thing. `analyze-trace.py` groups a
trace into gestures and reports what the model charged each one.

What the recording said, in order of how much it mattered:

1. **A zero vertical delta was cancelling the push.** All 46 `not-pushing` events in a recorded
   lean had `dy == 0.0` — not a downward move, but the normal report once the cursor is pinned at
   the edge. Charge plateaued at 0.384 s against a 0.45 s bar: unperformable, exactly as reported.
2. **The threshold was about as long as a whole stroke.** Longest unbroken push run: 43 events over
   0.655 s, median 8.3 ms apart, worth ~0.36 s of charge.
3. **The corner and the lean separate by 50x.** Corner pushes peaked at charge 0.021, a chrome lean
   reached 1.046. The threshold only has to sit somewhere sane in between; 0.25 s was picked at the
   felt end, and since charge accrues in real time it is also the delay the user experiences.
4. **The same physical event was arriving twice.** Consecutive lines, identical `dy`, identical
   `x`, 6 ms apart, one reporting `y = 982` and the other `y = 65`. The capture tap and the local
   monitor both fed the gesture and disagreed; the tap granted the ask, the monitor released it.
   The monitor was wrong — `locationInWindow` is relative to the window the event was *delivered*
   to, which stops being the view's window once the overlay re-parents it. This one is why the
   trace grew a `src=` tag: three rounds of inferring which path was which got it backwards once.

Traps this round, worth not repeating:

- **Don't re-simulate the model in the analyzer.** The first version did, and disagreed with the
  app because only some early-return branches reset the charge. The trace's own `push=`/`charge=`
  fields are ground truth and cost nothing.
- **A guard can be worse than the bug.** The first fix for the oscillation discarded events whose
  position disagreed with their delta — sound in principle, but with two producers poisoning each
  other's state it fired on **91% of events** and starved the gesture to one fire in a session.
- **`tail -f` on this log goes deaf.** The boot script `rm -f`s it every run, so a live monitor
  keeps following a dead inode and silently reports nothing. Use `tail -F`.

## Round 3 (2026-08-02): edge resistance itself, measured before touching it

The user's report was "the cursor resistance feels really weird", later narrowed to **the side
edges, while changing focus between displays**. Four faults, all confirmed *headlessly in about a
minute* — `EdgeResist::step` is pure, so the synthetic-event harness (`push.swift`, which needs
Accessibility for the posting shell) was unnecessary for every structural question. The
characterization tests live in `window/fit.rs` and are written to **flip** when the model is
fixed.

1. **A pointer on another display gets dragged back.** This is the reported symptom.
   `resist_edges` calls `resist.reset()` on every focus loss, which clears `through`. When the
   window becomes key again with the pointer parked on the external display, that pointer is
   hundreds of points past an edge — and `against` is `p >= hi - EPS`, trivially true — so
   outward motion is absorbed and the cursor warped home. Inward motion is unaffected, which is
   why it reads as erratic *jumping* rather than a wall. The design doc claims this class was
   fixed; the fix addressed the stale-position route, and focus churn reopens a second one.
2. **A flick breaks through; a deliberate push does not.** Breakthrough is pure distance with no
   per-event bound, and a flick arrives as ONE post-ballistics delta of 100+ pt while a careful
   push arrives as ten small ones. Same nominal 100 pt, opposite felt experience — and the
   careless motion the feature exists to stop is the one that sails through. `capture_step`
   already bounds overflow to the past-the-edge component; `EdgeResist` never learned that.
3. **A corner banks charge that fires the adjacent edge later.** One second of hot-corner dwell
   accumulates 600 pt on both axes (the corner arm accumulates but never releases). Sliding out
   along the top drains only `acc.0` — `inside_y` never clears the release margin — so a
   subsequent 1 pt nudge cashes 600 pt of charge from a gesture that ended seconds ago. A
   breakthrough causally disconnected from the motion that caused it is unattributable by
   definition, which is what "weird, can't say why" sounds like.
4. **Hugging any edge disarms every edge.** Re-arm requires clearance from all four edges at
   once, so ordinary guest work along a side panel leaves resistance off until the pointer
   moves away — silently.

Method notes worth carrying:

- **A pure model is its own oracle.** Two rounds of this spike drove real hardware to answer
  questions that `step` could have answered offline. Reach for the synthetic harness only for
  what genuinely needs the event stream (what deltas a real flick *produces* — still open).
- **Instrument before recording, and log the model's state, not just its inputs.** `[EDGE]` now
  carries `acc`, `through`, `thr`, `warp` and `outside` alongside the event. Without those an
  analyzer must re-derive them and will get the drain rules, the corner arm and the zero-delta
  early return wrong — the same trap round 2 hit.
- **Fix structure before tuning constants.** The 50/100/200 presets are denominated in
  post-ballistics points; bounding per-event push changes what all three mean, so any recording
  taken first would have to be thrown away.
- A fable agent reviewed the instrumentation plan and found (2) and (3), and killed two proposed
  metrics: "wasted travel" is unmeasurable (deltas are post-ballistics, and the post-warp
  suppression interval emits no events at all), and host-vs-guest cursor divergence is by design
  (the guest cursor is re-pinned every held event), so the meaningful guest-side measure is
  barrier charge delivered, not displacement.

## Tools

| file | what it does |
| --- | --- |
| `guest-corner-control.py` | uinput mouse into the corner, from inside the guest. **Run this first** — it is the control. |
| `push.swift` | repeatable synthetic shove posted to the session tap. Needs Accessibility *for the calling shell*, which an agent shell may not have. |
| `guest-watch.sh` | polls GNOME's `OverviewActive` and dumps the guest's relative-device traffic. |
| `LIMINA_EDGE_TRACE=1` | in-process log of every resistance decision (`[EDGE]`) and every chrome-ask decision (`[REVEAL]`), timestamped and tagged with which input path produced it. |
| `analyze-trace.py` | groups a trace into gestures and reports charge, push, strokes and why each event did not count. |

GNOME's own `OverviewActive` D-Bus property is the authoritative "did it fire" — no human
eyeballing a screen, no screenshot heuristics.

## Caveat on reading a dogfood trace

A trace captured while someone is *hunting* for a trigger contains deliberate extreme movements
alongside intended ones. Use it for structural facts — "this path never forwards", "this fires on
two events" — and not to fit constants to the movements in it. For tuning, record only agreed
intended gestures.
