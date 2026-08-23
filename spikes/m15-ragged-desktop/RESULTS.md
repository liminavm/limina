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

The 53 matter because that edge is at `y = 747`: the old filter's test was `top: r.y == 0`,
so it zeroed every upward push there. Before the fix, on the same rig, the cursor disappeared
when pushed past the built-in's bottom edge into the dead band.

**A cursor that vanishes at a monitor's *true* edge is not this bug.** The plane's hotspot
sits on the last scanline and the bitmap is clipped, so how much of it survives depends on
the cursor's size on that display. The tell is the echo: `guest pointer:` lines showed the
guest's own cursor holding at y=1438 of a 1440-row scanout across repeated pushes — pinned,
not lost. It is also fully drawn at a monitor's top edge, where the arrow points away from
the boundary.
