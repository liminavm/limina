// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Standalone global-IOSurface dumper: iosdump <id> [<id>...]
// Looks each global IOSurface up by id, GPU-coherent locks it, writes /tmp/ios-<id>.png.
import Foundation
import IOSurface
import CoreGraphics
import ImageIO
import UniformTypeIdentifiers

for arg in CommandLine.arguments.dropFirst() {
    guard let id = UInt32(arg) else { continue }
    guard let surf = IOSurfaceLookup(IOSurfaceID(id)) else {
        print("id=\(id) -> not alive"); continue
    }
    IOSurfaceLock(surf, .readOnly, nil)
    let w = IOSurfaceGetWidth(surf), h = IOSurfaceGetHeight(surf)
    let bpr = IOSurfaceGetBytesPerRow(surf)
    let base = IOSurfaceGetBaseAddress(surf)
    var nonzero = 0
    var uniform = true
    let p = base.assumingMemoryBound(to: UInt8.self)
    let first0 = p[0], first1 = p[1], first2 = p[2]
    // Build an RGBA buffer (force opaque) and tally.
    var rgba = [UInt8](repeating: 0, count: w*h*4)
    for y in 0..<h {
        for x in 0..<w {
            let s = y*bpr + x*4
            let b = p[s], g = p[s+1], r = p[s+2]
            if r|g|b != 0 { nonzero += 1 }
            if b != first0 || g != first1 || r != first2 { uniform = false }
            let o = (y*w + x)*4
            rgba[o]=r; rgba[o+1]=g; rgba[o+2]=b; rgba[o+3]=255
        }
    }
    IOSurfaceUnlock(surf, .readOnly, nil)
    let cs = CGColorSpaceCreateDeviceRGB()
    let ctx = CGContext(data: &rgba, width: w, height: h, bitsPerComponent: 8,
                        bytesPerRow: w*4, space: cs,
                        bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    let img = ctx.makeImage()!
    let url = URL(fileURLWithPath: "/tmp/ios-\(id).png")
    let dest = CGImageDestinationCreateWithURL(url as CFURL, UTType.png.identifier as CFString, 1, nil)!
    CGImageDestinationAddImage(dest, img, nil)
    CGImageDestinationFinalize(dest)
    print("id=\(id) \(w)x\(h) nonzero=\(nonzero) uniform=\(uniform) first=(\(first2),\(first1),\(first0)) -> /tmp/ios-\(id).png")
}
