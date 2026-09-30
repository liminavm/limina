// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva
//
// What tells trackpad pointer events from a mouse's, and a physical trackpad click from a
// tap-to-click, at the level limina's capture tap sees them (a session CGEventTap).
//
//   swiftc -O clickprobe.swift -o clickprobe && ./clickprobe [--seconds 90] > run.log
//
// Listen-only: logs, per event, the type, the mouse subtype field (kCGMouseEventSubtype), the
// pressure field, the click state, the NSEvent subtype and pressure/stage where AppKit gives
// them, and the local touch count of gesture events. Needs Accessibility (or Input Monitoring)
// for the terminal.

import AppKit
import CoreGraphics

var seconds = 90.0
var argv = CommandLine.arguments.dropFirst().makeIterator()
while let a = argv.next() {
    if a == "--seconds" { seconds = Double(argv.next() ?? "") ?? seconds }
}

let start = CFAbsoluteTimeGetCurrent()
func ms() -> String { String(format: "%9.1f", (CFAbsoluteTimeGetCurrent() - start) * 1000) }

var lastMove: (Int64, Int64)? = nil
var moves = 0

let types: [CGEventType] = [
    .mouseMoved, .leftMouseDown, .leftMouseUp, .rightMouseDown, .rightMouseUp,
    .leftMouseDragged, .rightMouseDragged, .otherMouseDown, .otherMouseUp, .scrollWheel,
]
var mask: CGEventMask = 0
for t in types { mask |= 1 << CGEventMask(t.rawValue) }
// NSEventTypeGesture (29), NSEventTypePressure (34), and the other gesture types.
for raw: UInt32 in [18, 19, 20, 29, 30, 31, 32, 34] { mask |= 1 << CGEventMask(raw) }

let callback: CGEventTapCallBack = { _, type, event, _ in
    let raw = type.rawValue
    let sub = event.getIntegerValueField(.mouseEventSubtype)
    let press = event.getDoubleValueField(.mouseEventPressure)
    let clicks = event.getIntegerValueField(.mouseEventClickState)
    let dev = event.getIntegerValueField(.mouseEventDeltaX)
    _ = dev
    let ns = NSEvent(cgEvent: event)
    switch raw {
    case CGEventType.mouseMoved.rawValue, CGEventType.leftMouseDragged.rawValue,
         CGEventType.rightMouseDragged.rawValue:
        // Collapse runs of identical motion lines; print each change of subtype.
        let key = (sub, Int64(raw))
        if lastMove == nil || lastMove! != key {
            print("\(ms()) MOVE type=\(raw) subtype=\(sub) pressure=\(press)")
            lastMove = key
        }
        moves += 1
    case 29, 30, 31, 32, 18, 19, 20:
        let n = ns.map { e in e.allTouches().filter { $0.device != nil && $0.phase != .ended && $0.phase != .cancelled }.count } ?? -1
        print("\(ms()) GESTURE type=\(raw) touches=\(n)")
    case 34:
        let stage = ns.map { e -> String in
            // -[NSEvent stage] is valid on pressure events.
            "\(e.stage) pressure=\(e.pressure) transition=\(e.stageTransition)"
        } ?? "?"
        print("\(ms()) PRESSURE stage=\(stage)")
    case CGEventType.scrollWheel.rawValue:
        // AppKit raises on -pressure for a scroll event: report the CG fields only.
        print("\(ms()) SCROLL subtype=\(sub)")
    default:
        let nsSub = ns.map { "\($0.subtype.rawValue)" } ?? "?"
        let nsPress = ns.map { "\($0.pressure)" } ?? "?"
        print("\(ms()) BUTTON type=\(raw) subtype=\(sub) nsSubtype=\(nsSub) pressure=\(press) nsPressure=\(nsPress) clicks=\(clicks)")
        lastMove = nil
    }
    fflush(stdout)
    return Unmanaged.passUnretained(event)
}

guard let tap = CGEvent.tapCreate(
    tap: .cgSessionEventTap, place: .headInsertEventTap, options: .listenOnly,
    eventsOfInterest: mask, callback: callback, userInfo: nil)
else {
    fputs("tapCreate failed: grant Accessibility / Input Monitoring to the terminal\n", stderr)
    exit(1)
}
let src = CFMachPortCreateRunLoopSource(nil, tap, 0)
CFRunLoopAddSource(CFRunLoopGetCurrent(), src, .commonModes)
CGEvent.tapEnable(tap: tap, enable: true)
print("\(ms()) START listening for \(seconds) s")
fflush(stdout)
DispatchQueue.main.asyncAfter(deadline: .now() + seconds) {
    print("\(ms()) END moves=\(moves)")
    exit(0)
}
CFRunLoopRun()
