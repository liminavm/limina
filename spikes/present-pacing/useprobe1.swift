// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// useprobe1.swift — single-shot variant of useprobe: swap layer contents ONCE,
// then (no further swaps) poll the replaced surface's use count until it clears.
// This avoids the double-buffer conflation in useprobe.swift (where "prev" went
// back onto the layer 16 ms later, so "never cleared" was an artifact).
// Repeats N times with a settle pause between rounds.
//
// Build/run:  swiftc -O useprobe1.swift -o useprobe1 && ./useprobe1

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
window.title = "useprobe1"
let view = NSView(frame: NSRect(x: 0, y: 0, width: W, height: H))
let layer = CALayer()
view.layer = layer
view.wantsLayer = true
window.contentView = view
window.makeKeyAndOrderFront(nil)
app.activate(ignoringOtherApps: true)

let surfA = makeSurface(w: W, h: H, color: (255, 0, 0))
let surfB = makeSurface(w: W, h: H, color: (0, 0, 255))

var round = 0
let ROUNDS = 20
var latencies: [Double] = []
var inUseAtAckCount = 0
var neverCleared = 0
var onLayer = 0 // 0 = A, 1 = B

func runRound() {
    round += 1
    if round > ROUNDS {
        let lat = latencies.sorted()
        func pct(_ p: Double) -> Double {
            lat.isEmpty ? 0 : lat[min(lat.count - 1, Int(Double(lat.count) * p))]
        }
        print("---")
        print("rounds: \(ROUNDS)")
        print("prev in-use at completion-block (ack): \(inUseAtAckCount)/\(ROUNDS)")
        if !lat.isEmpty {
            print(
                String(
                    format: "clear latency us: p50=%.0f p90=%.0f max=%.0f (n=%d)",
                    pct(0.5), pct(0.9), lat.last!, lat.count))
        }
        print("never cleared within 500ms: \(neverCleared)")
        app.terminate(nil)
        return
    }
    let next = onLayer == 0 ? surfB : surfA
    let prev = onLayer == 0 ? surfA : surfB
    onLayer ^= 1
    CATransaction.begin()
    CATransaction.setDisableActions(true)
    CATransaction.setCompletionBlock {
        let t0 = DispatchTime.now()
        let used0 = prev.isInUse
        if used0 { inUseAtAckCount += 1 }
        DispatchQueue.global().async {
            var cleared = !used0
            while DispatchTime.now().uptimeNanoseconds - t0.uptimeNanoseconds < 500_000_000 {
                if !prev.isInUse {
                    cleared = true
                    break
                }
                usleep(200)
            }
            let dtUs =
                Double(DispatchTime.now().uptimeNanoseconds - t0.uptimeNanoseconds) / 1000.0
            DispatchQueue.main.async {
                if used0 {
                    if cleared {
                        latencies.append(dtUs)
                        print(String(format: "round \(round): in-use at ack, cleared after %.0f us", dtUs))
                    } else {
                        neverCleared += 1
                        print("round \(round): in-use at ack, NEVER cleared (500ms cap)")
                    }
                } else {
                    print("round \(round): NOT in use at ack")
                }
                // settle, then next round
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) { runRound() }
            }
        }
    }
    layer.contents = next
    CATransaction.commit()
}

DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) { runRound() }
app.run()
