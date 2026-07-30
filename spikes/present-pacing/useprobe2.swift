// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// useprobe2.swift — steady-state fence-point probe for the ack SPLIT (§29/§30 follow-up).
//
// Question: where between "CA completion block" (latch) and "replaced surface off-glass"
// does the honest PRESENT moment live, and is there a pollable signal for it? Candidate:
// the moment IOSurfaceIsInUse(new) RISES (WindowServer picks the new frame up).
//
// Method: three surfaces round-robin at ~60 fps (steady state, like real presentation;
// three so the replaced surface stays off the layer for two frames — no useprobe.swift
// conflation). Per swap, relative to the commit:
//   t_latch  — when the CATransaction completion block fires
//   t_rise   — when isInUse(new) first reads true (polled from commit)
//   t_clear  — when isInUse(prev) first reads false (polled from commit)
//
// Build/run:  swiftc -O useprobe2.swift -o useprobe2 && ./useprobe2
import AppKit
import IOSurface
import QuartzCore

func makeSurface(w: Int, h: Int, color: (UInt8, UInt8, UInt8)) -> IOSurface {
    let props: [IOSurfacePropertyKey: Any] = [
        .width: w,
        .height: h,
        .bytesPerElement: 4,
        .pixelFormat: UInt32(0x42_47_52_41), // 'BGRA'
    ]
    let s = IOSurface(properties: props)!
    s.lock(options: [], seed: nil)
    let base = s.baseAddress.assumingMemoryBound(to: UInt8.self)
    for y in 0..<h {
        let row = base + y * s.bytesPerRow
        for x in 0..<w {
            row[x * 4 + 0] = color.2
            row[x * 4 + 1] = color.1
            row[x * 4 + 2] = color.0
            row[x * 4 + 3] = 0xFF
        }
    }
    s.unlock(options: [], seed: nil)
    return s
}

let app = NSApplication.shared
app.setActivationPolicy(.regular)

let W = 640, H = 400
let window = NSWindow(
    contentRect: NSRect(x: 200, y: 200, width: W, height: H),
    styleMask: [.titled], backing: .buffered, defer: false)
window.title = "useprobe2"
let view = NSView(frame: NSRect(x: 0, y: 0, width: W, height: H))
let layer = CALayer()
view.layer = layer
view.wantsLayer = true
window.contentView = view
window.makeKeyAndOrderFront(nil)
app.activate(ignoringOtherApps: true)

let surfs = [
    makeSurface(w: W, h: H, color: (255, 0, 0)),
    makeSurface(w: W, h: H, color: (0, 255, 0)),
    makeSurface(w: W, h: H, color: (0, 0, 255)),
]

let ROUNDS = 300  // ~5 s at 60 fps; first 60 warm-up rounds are discarded
var round = 0
var latchMs: [Double] = []
var riseMs: [Double] = []
var clearMs: [Double] = []
var riseBeforeLatch = 0
var riseNever = 0
var clearNever = 0
var measured = 0
let lock = NSLock()

func nowNs() -> UInt64 { DispatchTime.now().uptimeNanoseconds }

func swapOnce() {
    round += 1
    if round > ROUNDS {
        report()
        return
    }
    let next = surfs[round % 3]
    let prev = surfs[(round + 2) % 3]  // what's on the layer right now, being replaced
    let warm = round <= 60
    let tCommit = nowNs()
    var tLatch: UInt64 = 0

    CATransaction.begin()
    CATransaction.setDisableActions(true)
    CATransaction.setCompletionBlock {
        tLatch = nowNs()
    }
    layer.contents = next
    CATransaction.commit()

    if !warm {
        // Poll rise(new) and clear(prev) from commit on a background thread.
        DispatchQueue.global().async {
            var tRise: UInt64 = 0
            var tClear: UInt64 = 0
            let deadline = tCommit + 200_000_000
            while nowNs() < deadline, tRise == 0 || tClear == 0 {
                if tRise == 0, next.isInUse { tRise = nowNs() }
                if tClear == 0, !prev.isInUse { tClear = nowNs() }
                usleep(200)
            }
            lock.lock()
            measured += 1
            let latch = tLatch
            if latch > tCommit { latchMs.append(Double(latch - tCommit) / 1e6) }
            if tRise > 0 {
                riseMs.append(Double(tRise - tCommit) / 1e6)
                if latch > 0 && tRise < latch { riseBeforeLatch += 1 }
            } else {
                riseNever += 1
            }
            if tClear > 0 {
                clearMs.append(Double(tClear - tCommit) / 1e6)
            } else {
                clearNever += 1
            }
            lock.unlock()
        }
    }
    // ~60 fps cadence
    DispatchQueue.main.asyncAfter(deadline: .now() + 0.0166) { swapOnce() }
}

func report() {
    lock.lock()
    func stats(_ name: String, _ v: [Double]) {
        guard !v.isEmpty else {
            print("\(name): (none)")
            return
        }
        let s = v.sorted()
        func pct(_ p: Double) -> Double { s[min(s.count - 1, Int(Double(s.count) * p))] }
        print(
            String(
                format: "%@ ms after commit: p10=%.2f p50=%.2f p90=%.2f max=%.2f (n=%d)",
                name, pct(0.1), pct(0.5), pct(0.9), s.last!, s.count))
    }
    print("--- useprobe2: \(measured) measured swaps (60 warm-up discarded) ---")
    stats("latch (completion block)", latchMs)
    stats("rise  (isInUse(new) -> true)", riseMs)
    stats("clear (isInUse(prev) -> false)", clearMs)
    print("rise before latch: \(riseBeforeLatch); rise never within 200ms: \(riseNever); clear never: \(clearNever)")
    lock.unlock()
    app.terminate(nil)
}

DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) { swapOnce() }
app.run()
