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

## Tools

| file | what it does |
| --- | --- |
| `guest-corner-control.py` | uinput mouse into the corner, from inside the guest. **Run this first** — it is the control. |
| `push.swift` | repeatable synthetic shove posted to the session tap. Needs Accessibility *for the calling shell*, which an agent shell may not have. |
| `guest-watch.sh` | polls GNOME's `OverviewActive` and dumps the guest's relative-device traffic. |
| `LIMINA_EDGE_TRACE=1` | in-process log of every resistance decision and the overflow forwarded. |

GNOME's own `OverviewActive` D-Bus property is the authoritative "did it fire" — no human
eyeballing a screen, no screenshot heuristics.

## Caveat on reading a dogfood trace

A trace captured while someone is *hunting* for a trigger contains deliberate extreme movements
alongside intended ones. Use it for structural facts — "this path never forwards", "this fires on
two events" — and not to fit constants to the movements in it. For tuning, record only agreed
intended gestures.
