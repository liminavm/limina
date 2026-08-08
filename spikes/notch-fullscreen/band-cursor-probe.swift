// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva
//
// "The pointer vanishes when it enters the housing band" (reported 2026-08-08, `notch = extend`
// fullscreen). Input still tracks there — the guest reacts to hover — so only the drawn cursor is
// missing, and the guest holds a hardware cursor plane (checked in the guest's DRM state), which
// means the only pointer anywhere is the host `NSCursor`.
//
// This drives the pointer into the band so the `[CURSOR]` trace can report what we are wearing
// there (`LIMINA_EDGE_TRACE=1`). Three things it must get right, each learned the hard way:
//
//   * `CGWarpMouseCursorPosition` posts NO events. `on_motion` runs off `MouseMoved`, so a warp
//     produces zero trace lines whatever the mechanism — silence that looks like a finding and is
//     not one. These are real synthetic moves, posted to the HID tap.
//   * Run the CONTROL sweep first, in the middle of the guest. If the control is silent too, then
//     event posting is TCC-blocked and only a human can drive this; band silence means nothing
//     until the control has spoken.
//   * Stay off the top 2 pt. That is where the chrome ask arms, and granting it would drop the
//     strip mid-measurement — the probe would destroy what it came to look at. And sample away
//     from the middle of the panel: dead centre is behind the camera housing, where a human sees
//     no cursor no matter what the software does.
//
// Usage: swift band-cursor-probe.swift                       # list displays
//        swift band-cursor-probe.swift <display-index> control|band
import CoreGraphics
import Foundation

func displays() -> [CGDirectDisplayID] {
    var count: UInt32 = 0
    CGGetActiveDisplayList(0, nil, &count)
    var ids = [CGDirectDisplayID](repeating: 0, count: Int(count))
    CGGetActiveDisplayList(count, &ids, &count)
    return ids
}

let ids = displays()
guard CommandLine.arguments.count >= 3, let idx = Int(CommandLine.arguments[1]), idx < ids.count
else {
    for (i, id) in ids.enumerated() {
        let b = CGDisplayBounds(id)
        print(
            "[\(i)] id=\(id) bounds=(\(b.origin.x),\(b.origin.y) \(b.size.width)x\(b.size.height)) "
                + "builtin=\(CGDisplayIsBuiltin(id) != 0)")
    }
    exit(0)
}
let mode = CommandLine.arguments[2]
let bounds = CGDisplayBounds(ids[idx])
// CG global space is top-left origin: y=0 is the panel's top edge, so the band is its first ~33 pt.
let x = bounds.origin.x + bounds.size.width * 0.3
let (from, to): (CGFloat, CGFloat) =
    mode == "band" ? (140, 20) : (460, 380)

func move(_ p: CGPoint, _ d: CGFloat) {
    guard let e = CGEvent(mouseEventSource: nil, mouseType: .mouseMoved, mouseCursorPosition: p, mouseButton: .left)
    else { return }
    // Deltas are what the gesture code integrates; a move with none reads as "pinned".
    e.setIntegerValueField(.mouseEventDeltaY, value: Int64(d))
    e.setIntegerValueField(.mouseEventDeltaX, value: 0)
    e.post(tap: .cghidEventTap)
}

print("sweep \(mode): x=\(x) y \(from) -> \(to)")
var y = from
while y >= to {
    move(CGPoint(x: x, y: bounds.origin.y + y), -2)
    usleep(25_000)
    y -= 2
}
// Rest inside the target zone so the trace has a steady state to report, then report where the
// pointer actually ended up — if the system clamped or ignored the posts, this is the tell.
usleep(600_000)
let ev = CGEvent(source: nil)
print("resting at \(ev?.location ?? .zero)")
