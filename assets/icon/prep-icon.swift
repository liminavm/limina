// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// One-shot icon prep: re-grid the generated artwork onto Apple's app-icon grid.
// The source squircle is full-bleed (~938 of 1024 px); macOS icons sit on an
// 824x824 grid centered in a 1024 canvas, so scale content-bbox -> 824 and
// center it on a transparent canvas. The art keeps its own rounded shape and
// transparency; no clipping is applied.
//
// Usage: swift prep-icon.swift <in.png> <out.png>
import CoreGraphics
import ImageIO
import UniformTypeIdentifiers
import Foundation

let inURL = URL(fileURLWithPath: CommandLine.arguments[1]) as CFURL
let outURL = URL(fileURLWithPath: CommandLine.arguments[2]) as CFURL
let img = CGImageSourceCreateImageAtIndex(CGImageSourceCreateWithURL(inURL, nil)!, 0, nil)!
let w = img.width, h = img.height
let ctx0 = CGContext(data: nil, width: w, height: h, bitsPerComponent: 8, bytesPerRow: w*4,
    space: CGColorSpace(name: CGColorSpace.sRGB)!,
    bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
ctx0.draw(img, in: CGRect(x: 0, y: 0, width: w, height: h))
let buf = ctx0.data!.bindMemory(to: UInt8.self, capacity: w*h*4)
var minx = w, miny = h, maxx = 0, maxy = 0
for y in 0..<h { for x in 0..<w {
    let o = (y*w + x)*4
    if buf[o+3] > 10 && Int(buf[o]) + Int(buf[o+1]) + Int(buf[o+2]) > 42 {
        if x < minx { minx = x }; if x > maxx { maxx = x }
        if y < miny { miny = y }; if y > maxy { maxy = y }
    }
}}
let bw = Double(maxx - minx + 1), bh = Double(maxy - miny + 1)
let scale = 824.0 / max(bw, bh)
let outW = 1024, outH = 1024
let ctx = CGContext(data: nil, width: outW, height: outH, bitsPerComponent: 8, bytesPerRow: outW*4,
    space: CGColorSpace(name: CGColorSpace.sRGB)!,
    bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
ctx.interpolationQuality = .high
// Map the content-bbox center to the canvas center at the new scale. (CG origin
// is bottom-left; the bbox was computed in top-left raster coords — symmetric
// enough here that centering by bbox center is correct either way.)
let cx = (Double(minx) + Double(maxx) + 1.0) / 2.0
let cy = Double(h) - (Double(miny) + Double(maxy) + 1.0) / 2.0
let drawW = Double(w) * scale, drawH = Double(h) * scale
let ox = 512.0 - cx * scale, oy = 512.0 - cy * scale
ctx.draw(img, in: CGRect(x: ox, y: oy, width: drawW, height: drawH))
let out = ctx.makeImage()!
let dest = CGImageDestinationCreateWithURL(outURL, UTType.png.identifier as CFString, 1, nil)!
CGImageDestinationAddImage(dest, out, nil)
CGImageDestinationFinalize(dest)
print("wrote \(CommandLine.arguments[2]) (bbox \(Int(bw))x\(Int(bh)) -> 824 grid, scale \(scale))")
