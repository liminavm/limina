// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// linkprobe.swift — what an NSWindow's display link delivers on each screen: per callback the
// targetTimestamp, its gap to the previous one, and the link's `duration` (what vsync.rs takes as
// the nominal period). Run with the window on a ProMotion panel to see why a burst "read nothing".
//
//   ./linkprobe [screen-index] [callbacks] [rate] [draw]
//     rate: none (default range) | 120 | 60 | max
//     draw: none | every (swap the layer's IOSurface on every callback) | half (every other)
//   `screenlink` uses the screen's link, `cv` a CVDisplayLink on the screen, instead;
//   `fullscreen` takes the window natively fullscreen first.
//
// Build: swiftc -O linkprobe.swift -o linkprobe

import AppKit
import IOSurface
import QuartzCore

setlinebuf(stdout)
let args = CommandLine.arguments
let screenIndex = args.count > 1 ? Int(args[1])! : 0
let wanted = args.count > 2 ? Int(args[2])! : 120
let rate = args.count > 3 ? args[3] : "none"
let draw = args.count > 4 ? args[4] : "none"

let app = NSApplication.shared
app.setActivationPolicy(.regular)
let screens = NSScreen.screens
for (i, s) in screens.enumerated() {
    print("screen \(i): \(s.localizedName) max \(s.maximumFramesPerSecond) fps, min interval \(s.minimumRefreshInterval) max interval \(s.maximumRefreshInterval)")
}
let screen = screens[min(screenIndex, screens.count - 1)]
let f = screen.visibleFrame
let window = NSWindow(contentRect: NSRect(x: f.minX + 100, y: f.minY + 100, width: 400, height: 300),
                      styleMask: [.titled], backing: .buffered, defer: false, screen: screen)
window.title = "linkprobe"
let view = NSView(frame: NSRect(x: 0, y: 0, width: 400, height: 300))
let layer = CALayer()
layer.isOpaque = true
view.layer = layer
view.wantsLayer = true
window.contentView = view
func surface(_ v: UInt8) -> IOSurface {
    let s = IOSurface(properties: [.width: 400, .height: 300, .bytesPerElement: 4,
                                   .pixelFormat: UInt32(0x42_47_52_41)])!
    s.lock(options: [], seed: nil)
    memset(s.baseAddress, Int32(v), s.allocationSize)
    s.unlock(options: [], seed: nil)
    return s
}
let surfaces = [surface(0x20), surface(0xC0)]
window.setFrameOrigin(NSPoint(x: f.minX + 100, y: f.minY + 100))
window.makeKeyAndOrderFront(nil)
print("window on: \(window.screen?.localizedName ?? "none")")
app.activate(ignoringOtherApps: true)

final class Probe: NSObject {
    var last: CFTimeInterval = 0
    var n = 0
    @objc func step(_ link: CADisplayLink) {
        let t = link.targetTimestamp
        if last > 0 {
            print(String(format: "cb %d gap_ms=%.3f duration_ms=%.3f target-ts_ms=%.3f ts=%.6f",
                         n, (t - last) * 1000, link.duration * 1000,
                         (link.targetTimestamp - link.timestamp) * 1000, link.timestamp))
        }
        if draw == "every" || (draw == "half" && n % 2 == 0) {
            CATransaction.begin()
            CATransaction.setDisableActions(true)
            layer.contents = surfaces[n % 2 == 0 ? 0 : 1]
            CATransaction.commit()
        }
        last = t
        n += 1
        if n > wanted { app.terminate(nil) }
    }
}
// `fullscreen` anywhere in the arguments takes the window natively fullscreen first.
let fullscreen = args.contains("fullscreen")
if fullscreen {
    window.styleMask.insert(.resizable)
    window.collectionBehavior = [.fullScreenPrimary]
    window.toggleFullScreen(nil)
}
let probe = Probe()
// `screenlink` anywhere in the arguments takes the screen's link instead of the window's.
let link = args.contains("screenlink")
    ? screen.displayLink(target: probe, selector: #selector(Probe.step(_:)))
    : window.displayLink(target: probe, selector: #selector(Probe.step(_:)))
if args.contains("cv") {
    // CVDisplayLink on the screen's display: the timestamps' own period, printed per callback.
    let id = screen.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as! CGDirectDisplayID
    var cv: CVDisplayLink?
    CVDisplayLinkCreateWithCGDisplay(id, &cv)
    var lastHost: UInt64 = 0
    var count = 0
    CVDisplayLinkSetOutputHandler(cv!) { _, _, out, _, _ in
        let h = out.pointee.hostTime
        if lastHost > 0 {
            print(String(format: "cb %d gap_ms=%.3f", count, Double(h - lastHost) / 24_000.0))
        }
        lastHost = h
        count += 1
        if count > wanted { DispatchQueue.main.async { app.terminate(nil) } }
        return kCVReturnSuccess
    }
    CVDisplayLinkStart(cv!)
}
switch rate {
case "120": link.preferredFrameRateRange = CAFrameRateRange(minimum: 120, maximum: 120, preferred: 120)
case "60": link.preferredFrameRateRange = CAFrameRateRange(minimum: 60, maximum: 60, preferred: 60)
case "max": link.preferredFrameRateRange = CAFrameRateRange(minimum: 80, maximum: 120, preferred: 120)
default: break
}
if !args.contains("cv") {
    DispatchQueue.main.asyncAfter(deadline: .now() + (fullscreen ? 2.5 : 0.5)) {
        link.add(to: .main, forMode: .common)
    }
}
app.run()
