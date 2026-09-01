// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Planar-IOSurface dumper: iosdump-planar <id> [<id>...]
// The packed-RGBA iosdump misreads a two-plane '420f' decode target -- it walks
// 4 bytes per pixel over plane 0's luma. This one walks each plane at its own
// stride and writes /tmp/ios-<id>-p<n>.png (luma grey, chroma as R=U,G=V).
//
// The per-plane stride it prints is the load-bearing number: a writer and a
// reader disagreeing about it is the shear, and a surface whose planes are all
// zero says the decode never reached the surface at all -- which the guest-memory
// writeback checksum cannot see, because that is a different copy of the frame.
import Foundation
import IOSurface
import CoreGraphics
import ImageIO
import UniformTypeIdentifiers

func writeGray(_ pix: [UInt8], _ w: Int, _ h: Int, _ path: String) {
    var buf = pix
    let cs = CGColorSpaceCreateDeviceGray()
    guard let ctx = CGContext(data: &buf, width: w, height: h, bitsPerComponent: 8,
                              bytesPerRow: w, space: cs,
                              bitmapInfo: CGImageAlphaInfo.none.rawValue),
          let img = ctx.makeImage(),
          let dest = CGImageDestinationCreateWithURL(URL(fileURLWithPath: path) as CFURL,
                                                     UTType.png.identifier as CFString, 1, nil)
    else { return }
    CGImageDestinationAddImage(dest, img, nil)
    CGImageDestinationFinalize(dest)
}

for arg in CommandLine.arguments.dropFirst() {
    guard let id = UInt32(arg) else { continue }
    guard let surf = IOSurfaceLookup(IOSurfaceID(id)) else {
        print("id=\(id) -> not alive"); continue
    }
    IOSurfaceLock(surf, .readOnly, nil)
    let planes = IOSurfaceGetPlaneCount(surf)
    let fmt = IOSurfaceGetPixelFormat(surf)
    let fcc = String(bytes: [UInt8((fmt >> 24) & 0xff), UInt8((fmt >> 16) & 0xff),
                             UInt8((fmt >> 8) & 0xff), UInt8(fmt & 0xff)], encoding: .ascii) ?? "?"
    print("id=\(id) \(IOSurfaceGetWidth(surf))x\(IOSurfaceGetHeight(surf)) fourcc=\(fcc) planes=\(planes) alloc=\(IOSurfaceGetAllocSize(surf))")
    for pl in 0..<max(planes, 1) {
        let w = planes == 0 ? IOSurfaceGetWidth(surf) : IOSurfaceGetWidthOfPlane(surf, pl)
        let h = planes == 0 ? IOSurfaceGetHeight(surf) : IOSurfaceGetHeightOfPlane(surf, pl)
        let bpr = planes == 0 ? IOSurfaceGetBytesPerRow(surf) : IOSurfaceGetBytesPerRowOfPlane(surf, pl)
        let bpe = planes == 0 ? IOSurfaceGetBytesPerElement(surf) : IOSurfaceGetBytesPerElementOfPlane(surf, pl)
        let base = planes == 0 ? IOSurfaceGetBaseAddress(surf)
                               : IOSurfaceGetBaseAddressOfPlane(surf, pl)
        let p = base.assumingMemoryBound(to: UInt8.self)
        var nonzero = 0, mn = 255, mx = 0, sum = 0
        var grey = [UInt8](repeating: 0, count: w*h)
        for y in 0..<h {
            for x in 0..<w {
                let v = Int(p[y*bpr + x*bpe])   // first component of the element
                if v != 0 { nonzero += 1 }
                mn = min(mn, v); mx = max(mx, v); sum += v
                grey[y*w + x] = UInt8(v)
            }
        }
        let pct = Double(nonzero) * 100.0 / Double(w*h)
        print(String(format: "  plane %d: %dx%d bpr=%d (tight=%d, pad=%d) bpe=%d nonzero=%.1f%% min=%d max=%d mean=%.1f",
                     pl, w, h, bpr, w*bpe, bpr - w*bpe, bpe, pct, mn, mx, Double(sum)/Double(w*h)))
        writeGray(grey, w, h, "/tmp/ios-\(id)-p\(pl).png")
    }
    IOSurfaceUnlock(surf, .readOnly, nil)
}
