// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva
//
// Catch the notch strip drawing on the WRONG display, without a human watching for it.
//
// The `extend` strip is a 33 pt window over the camera housing on the built-in panel. During a
// Space switch it was seen flashing on the EXTERNAL display for a frame or two — far too brief
// and too intermittent to characterise by eye, and synthetic Space switches are ignored by the
// WindowServer, so the human has to drive while something else watches.
//
// It polls CGWindowListCopyWindowInfo (public API — no Screen Recording permission needed for
// bounds and alpha) every 5 ms and prints one line per *change* in a limina window's bounds,
// alpha or on-screen state. A change log rather than a sample dump because the question is never
// "where was it" but "what moved, and did our alpha change lead or trail it": a flash that is the
// WindowServer displacing a window we thought was parked needs a different fix from one where we
// reveal a window at a stale frame, and only the ordering tells them apart.
//
// Timestamps are absolute epoch milliseconds so lines interleave with the app's own
// LIMINA_OVERLAY_TRACE output. Displays are read from CoreGraphics rather than passed in — an
// earlier version took the expected origin on the command line and got it wrong, because
// CGWindowBounds is in CG's top-left-origin global space and NSScreen.frame is not.
//
//   swiftc -O spikes/notch-fullscreen/flash-detector.swift -o /tmp/flash-detector
//   /tmp/flash-detector            # log every change, flag the ones off the home display
//
// A window is "home" on the display holding most of its area. `OFF` marks any visible limina
// window whose home is not the display it started on — but read the summary, not the count:
//
//   A Space-switch slide legitimately carries **every** window of that Space across the display
//   boundary, so an off-display sample with the carrier at the same x is the animation, not a bug
//   (the window server clips it; nothing is seen). The signal is an off-display sample where the
//   strip moved **alone** — that is the window being drawn somewhere the guest is not. `SOLO` in
//   the summary counts exactly those, and it is the number that has to be zero.
//
// Ctrl-C prints the summary.

import CoreGraphics
import Foundation

struct Displays {
    var bounds: [(CGDirectDisplayID, CGRect)] = []

    init() {
        var ids = [CGDirectDisplayID](repeating: 0, count: 16)
        var n: UInt32 = 0
        CGGetActiveDisplayList(16, &ids, &n)
        for i in 0..<Int(n) { bounds.append((ids[i], CGDisplayBounds(ids[i]))) }
    }

    /// The display covering most of `r`, or nil if it touches none.
    func home(_ r: CGRect) -> CGDirectDisplayID? {
        var best: (CGDirectDisplayID, CGFloat)?
        for (id, b) in bounds {
            let i = b.intersection(r)
            guard !i.isNull else { continue }
            let area = i.width * i.height
            if area > (best?.1 ?? 0) { best = (id, area) }
        }
        return best?.0
    }

    func describe() -> String {
        bounds.map { id, b in
            "\(id)\(CGDisplayIsMain(id) != 0 ? "*" : "")=(\(Int(b.minX)),\(Int(b.minY)) \(Int(b.width))x\(Int(b.height)))"
        }.joined(separator: " ")
    }
}

let displays = Displays()
var last: [Int: String] = [:]  // window number -> last state string
var expectedHome: [Int: CGDirectDisplayID] = [:]  // where each window was first seen
var changes = 0
var off = 0
var solo = 0
/// Epoch-ms of the last sample in which *some* limina window was on its home display while another
/// was not. Windows sliding together are one event; a lone straggler is the bug.
var lastCompanion: Int64 = 0
var lastOff: Int64 = 0

func now() -> Int64 { Int64(Date().timeIntervalSince1970 * 1000.0) }

func scan() {
    guard
        let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID)
            as? [[String: Any]]
    else { return }
    var seen = Set<Int>()
    // Collected before anything is judged: whether an off-display window is the bug or just the
    // Space slide is decided by what the app's *other* windows are doing in the same sample.
    var visible: [(number: Int, rect: CGRect, alpha: Double, home: CGDirectDisplayID?, wrong: Bool)] = []
    for w in list {
        guard let owner = w[kCGWindowOwnerName as String] as? String, owner == "limina" else { continue }
        guard let b = w[kCGWindowBounds as String] as? [String: Any],
              let x = b["X"] as? Double, let y = b["Y"] as? Double,
              let width = b["Width"] as? Double, let height = b["Height"] as? Double,
              let number = w[kCGWindowNumber as String] as? Int
        else { continue }
        seen.insert(number)
        let alpha = (w[kCGWindowAlpha as String] as? Double) ?? 1.0
        let rect = CGRect(x: x, y: y, width: width, height: height)
        let home = displays.home(rect)
        // The first place a window is seen is where it belongs; anything else is the bug.
        if expectedHome[number] == nil, alpha > 0.01, let home { expectedHome[number] = home }
        let wrong = alpha > 0.01 && home != nil && expectedHome[number] != nil && home != expectedHome[number]
        visible.append((number, rect, alpha, home, wrong))
    }
    // Solo = something of ours is off its display while something else of ours, visible in the
    // same sample, is still at home. During a slide they travel together, so nothing is solo.
    let anyHome = visible.contains { $0.alpha > 0.01 && !$0.wrong && $0.home != nil }
    for v in visible {
        let isSolo = v.wrong && anyHome
        if v.wrong { lastOff = now() }
        if v.wrong && !isSolo { lastCompanion = now() }
        let mark = isSolo ? "SOLO" : (v.wrong ? "OFF " : "    ")
        let state =
            "\(mark) win=\(v.number) at=(\(Int(v.rect.minX)),\(Int(v.rect.minY)) "
            + "\(Int(v.rect.width))x\(Int(v.rect.height))) "
            + "alpha=\(String(format: "%.2f", v.alpha)) display=\(v.home.map(String.init) ?? "none")"
        if last[v.number] != state {
            last[v.number] = state
            changes += 1
            if v.wrong { off += 1 }
            if isSolo { solo += 1 }
            print("\(now()) \(state)")
            fflush(stdout)
        }
    }
    // A window leaving the on-screen list is itself a state change worth seeing: it is what
    // `orderOut` looks like from here, and distinguishing it from alpha=0 is the whole point.
    for (number, prev) in last where !seen.contains(number) && !prev.hasSuffix("GONE") {
        last[number] = "GONE"
        changes += 1
        print("\(now())     win=\(number) GONE (not in the on-screen list)")
        fflush(stdout)
    }
}

signal(SIGINT) { _ in
    print("\n--- \(changes) changes, \(off) off-display (\(solo) SOLO) ---")
    print(solo == 0
        ? "SOLO=0: nothing of ours was ever drawn off its display on its own."
        : "SOLO>0: a window was off its display while the rest of the app stayed home — the bug.")
    exit(solo == 0 ? 0 : 1)
}

print("\(now()) displays: \(displays.describe())")
fflush(stdout)
while true {
    scan()
    usleep(5000)  // 5 ms — finer than a WindowServer frame, so a one-frame flash cannot hide
}
