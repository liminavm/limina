// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Scan a range of global IOSurface IDs: print size/format/alpha stats for each live one.
// Usage: iosprobe <fromId> <toId>   — small surfaces are cursor candidates.
import Foundation
import IOSurface

let args = CommandLine.arguments
let lo = args.count > 1 ? UInt32(args[1]) ?? 1 : 1
let hi = args.count > 2 ? UInt32(args[2]) ?? 500 : 500
for id in lo...hi {
    guard let surf = IOSurfaceLookup(IOSurfaceID(id)) else { continue }
    let w = IOSurfaceGetWidth(surf), h = IOSurfaceGetHeight(surf)
    let fmt = IOSurfaceGetPixelFormat(surf)
    let fmtStr = String(bytes: [UInt8((fmt >> 24) & 0xff), UInt8((fmt >> 16) & 0xff),
                                UInt8((fmt >> 8) & 0xff), UInt8(fmt & 0xff)], encoding: .ascii) ?? "?"
    var alphaMin: UInt8 = 255, alphaMax: UInt8 = 0, nonzero = 0
    if w * h <= 1024 * 1024 {
        IOSurfaceLock(surf, .readOnly, nil)
        let bpr = IOSurfaceGetBytesPerRow(surf)
        let p = IOSurfaceGetBaseAddress(surf).assumingMemoryBound(to: UInt8.self)
        for y in 0..<h {
            for x in 0..<w {
                let s = y * bpr + x * 4
                let a = p[s + 3]
                alphaMin = min(alphaMin, a); alphaMax = max(alphaMax, a)
                if p[s] | p[s + 1] | p[s + 2] != 0 { nonzero += 1 }
            }
        }
        IOSurfaceUnlock(surf, .readOnly, nil)
    }
    print("id=\(id) \(w)x\(h) fmt=\(fmtStr) alpha=[\(alphaMin),\(alphaMax)] nonzeroRGB=\(nonzero)/\(w*h)")
}
