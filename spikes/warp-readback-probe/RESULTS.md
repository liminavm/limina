# Warp readback probe

**Question.** After `CGWarpMouseCursorPosition`, can the cursor's new position be read back
*immediately* — synchronously, in the same call stack — so a post-warp assertion is sound? And
does the answer hold with the mouse disassociated (`CGAssociateMouseAndMouseCursorPosition(0)`,
the captured state) and across displays?

**Vehicle.** `probe.swift`: read the position, warp to the centre of the *other* display (or 200 pt
away on a one-display Mac), read back at once via both `NSEvent.mouseLocation` and a fresh
`CGEvent(source: nil).location`, restore; repeat with association off.

```
swiftc -O -o probe probe.swift && ./probe
```

**Result (measured 2026-08-21, macOS 26.5, two displays: main 2560×1440 at (0,0), built-in
1512×982 at (−1512, 747)).** The readback equals the warp target, on both APIs, within ~0.4 ms of
the warp, with association on and with it off, for a cross-display warp. The warp is synchronous
with respect to position queries. The probe's targets were integer display centres; the live
assertion then showed the readback is **floored to whole points** (warp to (1746.9, 259.8) →
readback (1746.0, 259.0)), so "equals" holds to the point, not the sub-point.

**Consequence.** `window/warp.rs` asserts after every warp that the cursor reads back at the
target (`LANDING_TOLERANCE` = 1.5 pt: the floor's √2 plus dust). A miss is the window server's
clamp into the display union — the target was off every display — never a race with the readback.
