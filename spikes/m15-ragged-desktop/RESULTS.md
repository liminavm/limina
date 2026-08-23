# A guest desktop that is not a rectangle

The guest spreads its one absolute pointer device over the **bounding box** of its desktop.
A desktop is a union of rectangles, so any vertical offset or height mismatch between two
monitors leaves corners of that box belonging to no monitor. Two host-side mechanisms
assumed the box *was* the desktop, and both are wrong the moment it is not:

- the captured pointer's range clamp, which pinned at the range's ends — the box's corners,
  not the desktop's — so a monitor that does not reach the box's edge could be pushed clean
  off itself into dead space, where nothing draws a cursor plane and the cursor is gone;
- the edge-pressure filter, which called an edge outer only when it sat at a bounding-box
  coordinate, so an offset monitor's leading edges read as seams and the pressure they were
  owed was dropped.

Both are fixed in `window/arrangement.rs` (`Desktop::confine`, `outer_edges_at`) and
`window/fit.rs` (`range_step`).

## The oracle

`dead-space-check.py` replays every absolute position we put on the wire against the guest's
own reported rects. Run the VM with `LIMINA_POINTER_WIRE_TRACE=1`, read the guest's logical
rects out of its compositor, and feed both in:

```
LIMINA_DISK=<enhanced.raw> LIMINA_NET=1 LIMINA_POINTER_WIRE_TRACE=1 \
    spikes/venus-draw-probe/boot-enhanced-efi-kk.sh
# in the guest: busctl --user call org.gnome.Mutter.DisplayConfig \
#     /org/gnome/Mutter/DisplayConfig org.gnome.Mutter.DisplayConfig GetCurrentState
spikes/m15-ragged-desktop/dead-space-check.py /tmp/limina-worker-<disk>.log \
    1512,0,2048,1152 0,747,1512,948
```

**Read the extremes it prints before believing the zero.** A run where the hand never drove
the pointer into a wall reports zero dead-space positions for the wrong reason.

To arrange a guest ragged on purpose, apply a config with a vertical offset over DBus
(`ApplyMonitorsConfig`, method 2 to persist). The relay will often produce a ragged guest by
itself where the host's own panels are offset, which is how this reached the dogfood.

## Measured on the two-panel rig, 2026-08-23

Guest arranged BenQ `(1512,0) 2048x1152`, built-in `(0,747) 1512x948` — box 3560x1695, with
dead space above the built-in and below the BenQ.

| | |
|---|---|
| absolute positions sent | 2374 |
| landing on no monitor | **0** |
| deepest reach on the BenQ | y = 1152.0, its bottom edge exactly |
| shallowest reach on the built-in | y = 747.0, its top edge exactly |
| upward pressure events charged at the built-in's top | 53 |

**These numbers verify the confinement, not the edge filter.** The run was fullscreen, so it
was captured throughout, and the captured path takes its pressure from the confinement's own
clamp — it never consults `outer_edges_at`. The old code was silent at that same wall because
its clamp was the box's end at range `y = 0`, nowhere near the monitor's top at `y = 747`, so
there was no clamped-off motion to charge with. Before the fix, on the same rig, the cursor
disappeared when pushed past the built-in's bottom edge into the dead band.

`outer_edges_at` governs the **uncaptured** path (and the captured seed, before a running range
exists). Its end-to-end differential is uncaptured **hover** past an edge facing dead space — plain
motion, no button. There is no uncaptured drag to use instead: where the tap is installed it
owns clicks and takes the grab, so a press captures.

### The per-point edge, measured

Same physical action, same corner, same session, pointer free throughout — only the guest's
arrangement differs. `Virtual-1` (primary, 2048x1152) is held at `(0,948)` in both, so under
the old per-side test its top edge was never outer (`r.y == 0` is false at 948) and every
upward push was dropped whatever the neighbour did.

| the corner under the push | upward pressure | absolute samples | overview |
| --- | --- | --- | --- |
| **wall** — `Virtual-2` at `(1024,0)`, above the RIGHT half | **96** | 1689 | opens |
| **seam** — `Virtual-2` at `(0,0)`, above the LEFT half | **0** | 3033 | opens |

Neither window contains a capture transition, so both are the uncaptured path throughout.
The wall column is the differential against the old code; the seam column is the same edge
refusing to charge where a neighbour actually abuts it, which is the property no per-side
answer can express.

**The hot corner alone does not prove pressure arrived.** The overview opens in *both*
arrangements — and in the seam case it opened with **zero** pressure events on the wire, so
opening it is not evidence that any was delivered. What does differ is the effort: with the
pressure the corner triggers readily, and without it the operator reports having to work at it,
which reads as a second, harder route still being available to an absolute pointer that lands
in the corner itself. Which route that is was not confirmed — `gresource` is absent from the
guest, so the shell's own implementation was not read. Treat the overview as a visible sanity
check on the wall case and nothing more; the wire trace is what tells the two halves apart.
(Captured is the case a barrier genuinely cannot serve on its own — there the absolute stream is
pre-clamped, which is why the pressure device exists at all.)

Multi-display exists only in `FullscreenAll`: windowed mode hands the guest exactly one
connector (`displays.rs`, `wanted`). So the test is fullscreen with the grab released, never
windowed. Releasing takes **two** `Cmd-Ctrl-G` — the first promotes to a hard grab.

Two edges are unusable on this rig, both eaten by macOS before the app sees them: the **right**
edge, where Universal Control hands the cursor to another Mac (measured: the pointer stalls
~9 units short and nothing charges), and the **bottom**, where the Dock reveals. **Any test run fullscreen is a test
of the captured path, whatever it was meant to test** — `docs/input-and-windows.md` §4.

**A cursor that vanishes at a monitor's *true* edge is not this bug.** The plane's hotspot
sits on the last scanline and the bitmap is clipped, so how much of it survives depends on
the cursor's size on that display. The tell is the echo: `guest pointer:` lines showed the
guest's own cursor holding at y=1438 of a 1440-row scanout across repeated pushes — pinned,
not lost. It is also fully drawn at a monitor's top edge, where the arrow points away from
the boundary.
