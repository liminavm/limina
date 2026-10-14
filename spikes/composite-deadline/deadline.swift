// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// deadline.swift — how long before a vsync must a layer-contents commit land for WindowServer to
// show it at that vsync? (docs/design/host-vblank.md: the lead's composite deadline `w`.)
//
// Each round waits for a display-link callback, spins until `target - d` (target = the link's
// targetTimestamp, the coming vsync), swaps the layer's IOSurface contents the way the supervisor
// does (`present.rs::set_layer_surface`: a CATransaction with actions disabled), and tight-polls
// the replaced surface until WindowServer lets go of it. One line per round on stdout:
//
//   round <n> d_ms=<phase before target> c_ms=<commit - target> u_ms=<release - target> period_ms=<p>
//
// Modes:
//   ./deadline fixed <d_ms> <rounds> [fullscreen]   one phase: is the release oracle per-vsync?
//   ./deadline sweep <passes> [lo hi step] [fullscreen]  phases lo..hi ms (default 0.5..15.5 by
//                                                         0.5), shuffled each pass; a negative
//                                                         phase commits AFTER the target
//
// Build: swiftc -O deadline.swift -o deadline

import AppKit
import IOSurface
import QuartzCore

func makeSurface(w: Int, h: Int, color: (UInt8, UInt8, UInt8)) -> IOSurface {
    let props: [IOSurfacePropertyKey: Any] = [
        .width: w, .height: h, .bytesPerElement: 4, .pixelFormat: UInt32(0x42_47_52_41),
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

var timebase = mach_timebase_info_data_t()
mach_timebase_info(&timebase)
func ticksToMs(_ t: Int64) -> Double { Double(t) * Double(timebase.numer) / Double(timebase.denom) / 1e6 }
func secsToTicks(_ s: Double) -> UInt64 { UInt64(s * 1e9 * Double(timebase.denom) / Double(timebase.numer)) }

let args = CommandLine.arguments
let mode = args.count > 1 ? args[1] : "fixed"
var phases: [Double] = []
if mode == "sweep" {
    let passes = args.count > 2 ? Int(args[2])! : 10
    let lo = args.count > 3 ? Double(args[3])! : 0.5
    let hi = args.count > 4 ? Double(args[4])! : 15.5
    let step = args.count > 5 ? Double(args[5])! : 0.5
    let base = stride(from: lo, through: hi, by: step).map { $0 }
    for _ in 0..<passes { phases += base.shuffled() }
} else {
    let d = args.count > 2 ? Double(args[2])! : 12.0
    let n = args.count > 3 ? Int(args[3])! : 60
    phases = Array(repeating: d, count: n)
}
let fullscreen = args.contains("fullscreen")

let app = NSApplication.shared
app.setActivationPolicy(.regular)
let W = 1280, H = 800
let window = NSWindow(
    contentRect: NSRect(x: 100, y: 100, width: W, height: H),
    styleMask: [.titled, .resizable], backing: .buffered, defer: false)
window.title = "composite-deadline"
window.collectionBehavior = [.fullScreenPrimary]
let view = NSView(frame: NSRect(x: 0, y: 0, width: W, height: H))
let layer = CALayer()
layer.isOpaque = true
view.layer = layer
view.wantsLayer = true
view.layerContentsRedrawPolicy = .never
window.contentView = view
window.makeKeyAndOrderFront(nil)
app.activate(ignoringOtherApps: true)
if fullscreen { window.toggleFullScreen(nil) }

let surfaces = [
    makeSurface(w: W, h: H, color: (255, 0, 0)),
    makeSurface(w: W, h: H, color: (0, 0, 255)),
]
var onLayer = 0
layer.contents = surfaces[0]

final class Prober: NSObject {
    var round = 0
    var armed = false
    var link: CADisplayLink?

    @objc func step(_ link: CADisplayLink) {
        guard armed else { return }
        armed = false
        link.isPaused = true
        round += 1
        if round > phases.count {
            print("done")
            app.terminate(nil)
            return
        }
        let d = phases[round - 1]
        let period = link.targetTimestamp - link.timestamp
        let target = secsToTicks(link.targetTimestamp)
        let at = d >= 0 ? target &- secsToTicks(d / 1000) : target &+ secsToTicks(-d / 1000)
        while mach_absolute_time() < at {}
        let prev = surfaces[onLayer]
        onLayer ^= 1
        let c = mach_absolute_time()
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        layer.contents = surfaces[onLayer]
        CATransaction.commit()
        let r = round
        DispatchQueue.global(qos: .userInteractive).async {
            // Wait for WindowServer to take the new contents, then for it to let go of the old.
            var u: UInt64 = 0
            let deadline = c + secsToTicks(0.2)
            while mach_absolute_time() < deadline {
                if !prev.isInUse { u = mach_absolute_time(); break }
                usleep(50)
            }
            let line = String(
                format: "round %d d_ms=%.2f c_ms=%.3f u_ms=%.3f period_ms=%.4f", r, d,
                ticksToMs(Int64(bitPattern: c &- target)),
                u == 0 ? 999.0 : ticksToMs(Int64(bitPattern: u &- target)), period * 1000)
            DispatchQueue.main.async {
                print(line)
                fflush(stdout)
                // Settle: the next round's surface must be off glass and released.
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.25) {
                    self.armed = true
                    self.link?.isPaused = false
                }
            }
        }
    }
}

let prober = Prober()
let link = window.displayLink(target: prober, selector: #selector(Prober.step(_:)))
prober.link = link
link.add(to: .main, forMode: .common)
DispatchQueue.main.asyncAfter(deadline: .now() + (fullscreen ? 2.5 : 1.0)) {
    prober.armed = true
}
app.run()
