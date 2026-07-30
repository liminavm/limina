// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// useprobe.swift — does WindowServer hold an IOSurface use count while a plain
// CALayer.contents surface is being read, and how long past the CATransaction
// completion block does the PREVIOUS surface stay in use?
//
// Context (#24 zero-copy reuse tear): the supervisor acks "shown X" from the
// CATransaction completion block; the worker completes the guest flush fence on
// that ack; the guest then repaints buffer X-1. If WindowServer is still
// sampling X-1 at that instant, mid-repaint states reach glass. This probe
// measures the ground truth on the host, no VM involved:
//   1. alternate two IOSurfaces as layer contents at ~60 Hz,
//   2. at each transaction's completion block, poll IOSurfaceIsInUse(prev)
//      until it clears, and record how long that took.
//
// Build/run:  swiftc -O useprobe.swift -o useprobe && ./useprobe
// Output: per-swap "in-use at ack" flag + clear latency; summary histogram.

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
window.title = "useprobe"
let view = NSView(frame: NSRect(x: 0, y: 0, width: W, height: H))
let layer = CALayer()
view.layer = layer
view.wantsLayer = true
window.contentView = view
window.makeKeyAndOrderFront(nil)
app.activate(ignoringOtherApps: true)

let surfA = makeSurface(w: W, h: H, color: (255, 0, 0))
let surfB = makeSurface(w: W, h: H, color: (0, 0, 255))

var swapCount = 0
var inUseAtAck = 0
var clearLatenciesUs: [Double] = []
var neverCleared = 0
var current = 0 // which surface is on the layer

func snapshotUse(_ s: IOSurface) -> (Bool, Int32) {
    (s.isInUse, s.localUseCount)
}

func swapOnce() {
    let next = current == 0 ? surfB : surfA
    let prev = current == 0 ? surfA : surfB
    current ^= 1
    swapCount += 1
    let n = swapCount
    CATransaction.begin()
    CATransaction.setDisableActions(true)
    CATransaction.setCompletionBlock {
        // This is the ack instant in limina. Is the PREVIOUS surface still in use?
        let t0 = DispatchTime.now()
        let (used0, _) = snapshotUse(prev)
        if used0 { inUseAtAck += 1 }
        // Poll off the main thread until it clears (cap 100 ms).
        DispatchQueue.global().async {
            var cleared = false
            while DispatchTime.now().uptimeNanoseconds - t0.uptimeNanoseconds < 100_000_000 {
                if !prev.isInUse {
                    cleared = true
                    break
                }
                usleep(500)
            }
            let dtUs =
                Double(DispatchTime.now().uptimeNanoseconds - t0.uptimeNanoseconds) / 1000.0
            DispatchQueue.main.async {
                if used0 {
                    if cleared {
                        clearLatenciesUs.append(dtUs)
                    } else {
                        neverCleared += 1
                    }
                }
                if n <= 20 || n % 60 == 0 {
                    print(
                        "swap \(n): prev in-use at ack=\(used0)"
                            + (used0
                                ? cleared
                                    ? String(format: " cleared after %.0f us", dtUs)
                                    : " NEVER cleared (100ms cap)"
                                : ""))
                }
            }
        }
    }
    layer.contents = next
    CATransaction.commit()
}

let SWAPS = 300
var timerTicks = 0
let timer = Timer.scheduledTimer(withTimeInterval: 1.0 / 60.0, repeats: true) { t in
    timerTicks += 1
    if timerTicks > SWAPS {
        t.invalidate()
        // Let stragglers finish, then summarize.
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) {
            let lat = clearLatenciesUs.sorted()
            func pct(_ p: Double) -> Double {
                lat.isEmpty ? 0 : lat[min(lat.count - 1, Int(Double(lat.count) * p))]
            }
            print("---")
            print("swaps: \(SWAPS)")
            print(
                "prev surface STILL IN USE at completion-block (ack) time: "
                    + "\(inUseAtAck)/\(SWAPS)")
            if !lat.isEmpty {
                print(
                    String(
                        format:
                            "clear latency us: p50=%.0f p90=%.0f p99=%.0f max=%.0f (n=%d)",
                        pct(0.5), pct(0.9), pct(0.99), lat.last!, lat.count))
            }
            print("never cleared within 100ms: \(neverCleared)")
            app.terminate(nil)
        }
    } else {
        swapOnce()
    }
}
RunLoop.main.add(timer, forMode: .common)
app.run()
