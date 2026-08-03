# Recordings

Raw `[EDGE]` / `[REVEAL]` traces from real sessions, kept because they are expensive to reproduce
(they need a booted VM, a second display, and a human performing labelled intents) and cheap to
store. Read them with `../analyze-resist.py` / `../analyze-trace.py`.

| file | what |
| --- | --- |
| `2026-08-02-resist-take1.log` | Edge resistance under commit `976194f` (the three model fixes), on dev-mac with an external display. Seven labelled takes: guest top bar, hot corner, deliberate right-edge exits, wandering on the external display, a focus round trip, a drag to an edge, idle wiggle. **This is the recording that killed the design**: 1133 `escaped=true` events with `warp=0.0` prove the pointer is no longer dragged off the other display, while the side edges show almost no held events at all — crossing never lands ON the edge, so the escape guard fired on the first event of every exit. That is what pushed us to the pointer-grab pivot (`docs/design/fullscreen-pointer-grab.md`). |

Note the trace's `t=` is milliseconds since the first traced event of that process, not wall clock.
