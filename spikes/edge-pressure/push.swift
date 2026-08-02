// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Synthesize a sustained pointer shove at a screen edge or corner, so the edge-resistance
// path can be exercised without a human pushing a mouse.
//
// Why this exists: the GNOME hot corner would not fire through resistance, and every theory
// about why ("the rounded display corner", "mutter ignores the relative device") was
// untestable by hand — a shove is not reproducible, and the one time it worked could not be
// repeated. This makes the input exactly repeatable, which turns the question into arithmetic:
// the tap absorbs N points and forwards them, mutter's barrier needs 100 px, so either we
// deliver 100 before we let go or we do not.
//
// The events are posted to the session tap (`CGEventPost(.cgSessionEventTap)`), which is where
// `capture_tap` head-inserts itself, so they take exactly the path a real mouse's do — including
// `kCGMouseEventDeltaX/Y`, which is all the resistance measures pressure in.
//
//   swiftc -O -o push push.swift
//   ./push --x 1 --y 1 --dx -10 --dy -10 --count 20 --interval 8
//
// Coordinates are CG global (top-left origin). Use `--warp-only` to just place the cursor.

import CoreGraphics
import Foundation

func arg(_ name: String, _ fallback: Double) -> Double {
    let a = CommandLine.arguments
    guard let i = a.firstIndex(of: "--\(name)"), i + 1 < a.count, let v = Double(a[i + 1]) else {
        return fallback
    }
    return v
}
let flag = { (n: String) in CommandLine.arguments.contains("--\(n)") }

let x = arg("x", 1), y = arg("y", 1)
let dx = arg("dx", -10), dy = arg("dy", -10)
let count = Int(arg("count", 20)), interval = arg("interval", 8) / 1000.0

// Land the cursor on the target first. The resistance reads the event's own location, so the
// warp is what puts us "against" the edge; the deltas are then pure pressure.
CGWarpMouseCursorPosition(CGPoint(x: x, y: y))
// A warp opens a ~0.25 s local-events suppression interval during which posted motion is
// ignored — the same trap the resistance itself hit. End it before pushing, or the first
// third of the shove silently evaporates.
CGAssociateMouseAndMouseCursorPosition(1)
if flag("warp-only") { exit(0) }
Thread.sleep(forTimeInterval: 0.05)

for i in 0..<count {
    // Hold the reported location at the target: at a real screen edge the cursor stops moving
    // while the deltas keep coming, and that is precisely the state being reproduced.
    guard
        let e = CGEvent(
            mouseEventSource: nil, mouseType: .mouseMoved,
            mouseCursorPosition: CGPoint(x: x, y: y), mouseButton: .left)
    else { exit(1) }
    e.setDoubleValueField(.mouseEventDeltaX, value: dx)
    e.setDoubleValueField(.mouseEventDeltaY, value: dy)
    e.post(tap: .cgSessionEventTap)
    FileHandle.standardError.write("push \(i + 1)/\(count) at (\(x),\(y)) d=(\(dx),\(dy))\n".data(using: .utf8)!)
    Thread.sleep(forTimeInterval: interval)
}
