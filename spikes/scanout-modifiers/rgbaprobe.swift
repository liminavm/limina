// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// rgbaprobe — does WindowServer correctly display an 'RGBA'-fourcc IOSurface set
// directly as CALayer.contents (layer-hosting, like limina's presenter)?
//
// Shows a window with two panels carrying the SAME logical test pattern:
//   TOP    'BGRA' IOSurface (reference — the golden path limina uses today)
//   BOTTOM 'RGBA' IOSurface (the candidate for XBGR8888/ABGR8888 primary-plane support)
// Pattern (both panels): thin GREEN strip on top, RED left half, BLUE right half.
// If WindowServer handles 'RGBA', the panels are identical. A red/blue swap in the
// bottom panel means it misreads the byte order; black/garbage means unsupported.
//
// Build: swiftc -O rgbaprobe.swift -o rgbaprobe
// Run:   ./rgbaprobe   (window stays up until closed / process killed)

import AppKit
import IOSurface

let W = 400, H = 200

func fourcc(_ s: String) -> UInt32 {
    s.utf8.reduce(0) { ($0 << 8) | UInt32($1) }
}

// order: byte layout per pixel for (r,g,b,a)
func makeSurface(pixelFormat: UInt32, pack: (UInt8, UInt8, UInt8, UInt8) -> [UInt8]) -> IOSurface {
    let props: [IOSurfacePropertyKey: Any] = [
        .width: W, .height: H, .bytesPerElement: 4, .pixelFormat: pixelFormat,
    ]
    let surf = IOSurface(properties: props)!
    surf.lock(options: [], seed: nil)
    let base = surf.baseAddress.assumingMemoryBound(to: UInt8.self)
    let stride = surf.bytesPerRow
    for y in 0..<H {
        for x in 0..<W {
            let (r, g, b): (UInt8, UInt8, UInt8) =
                y < 24 ? (0, 255, 0) : (x < W / 2 ? (255, 0, 0) : (0, 0, 255))
            let px = pack(r, g, b, 255)
            let off = y * stride + x * 4
            for i in 0..<4 { base[off + i] = px[i] }
        }
    }
    surf.unlock(options: [], seed: nil)
    return surf
}

// 'BGRA' fourcc = little-endian B,G,R,A in memory
let bgra = makeSurface(pixelFormat: fourcc("BGRA")) { r, g, b, a in [b, g, r, a] }
// 'RGBA' fourcc = R,G,B,A in memory (what a Vulkan R8G8B8A8 / DRM XBGR8888 buffer holds)
let rgba = makeSurface(pixelFormat: fourcc("RGBA")) { r, g, b, a in [r, g, b, a] }

let app = NSApplication.shared
app.setActivationPolicy(.regular)

let win = NSWindow(
    contentRect: NSRect(x: 200, y: 200, width: W + 20, height: 2 * H + 30),
    styleMask: [.titled, .closable], backing: .buffered, defer: false)
win.title = "rgbaprobe — top: BGRA ref, bottom: RGBA probe"
let view = NSView(frame: win.contentView!.bounds)
let root = CALayer()
view.layer = root
view.wantsLayer = true
view.layerContentsRedrawPolicy = .never

func panel(_ surf: IOSurface, y: CGFloat) -> CALayer {
    let l = CALayer()
    l.frame = CGRect(x: 10, y: y, width: CGFloat(W), height: CGFloat(H))
    l.isOpaque = true
    l.contents = surf
    return l
}
// CALayer origin is bottom-left: bottom panel = RGBA probe, top panel = BGRA reference
root.addSublayer(panel(rgba, y: 10))
root.addSublayer(panel(bgra, y: CGFloat(H) + 20))

win.contentView = view
win.makeKeyAndOrderFront(nil)
win.level = .floating
app.activate(ignoringOtherApps: true)
print("window up: top = BGRA reference, bottom = RGBA probe (both should show green strip, red left, blue right)")
app.run()
