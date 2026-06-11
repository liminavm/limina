// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Extract frames around a timestamp from a screen recording and print per-frame
// stats (mean RGB of the frame center region) to locate anomaly frames.
// usage: swift extract-frames.swift <movie> <start-sec> <end-sec> <fps> <outdir>
import AVFoundation
import AppKit

let args = CommandLine.arguments
guard args.count == 6 else { print("usage: <movie> <start> <end> <fps> <outdir>"); exit(1) }
let url = URL(fileURLWithPath: args[1])
let start = Double(args[2])!, end = Double(args[3])!, fps = Double(args[4])!
let outdir = args[5]
try? FileManager.default.createDirectory(atPath: outdir, withIntermediateDirectories: true)

let asset = AVAsset(url: url)
let gen = AVAssetImageGenerator(asset: asset)
gen.requestedTimeToleranceBefore = .zero
gen.requestedTimeToleranceAfter = .zero
gen.appliesPreferredTrackTransform = true

var t = start
while t <= end {
    let time = CMTime(seconds: t, preferredTimescale: 6000)
    guard let cg = try? gen.copyCGImage(at: time, actualTime: nil) else { t += 1.0/fps; continue }
    // mean RGB over the central 60% of the frame
    let w = cg.width, h = cg.height
    let cw = Int(Double(w)*0.6), ch = Int(Double(h)*0.6)
    let cx = (w-cw)/2, cy = (h-ch)/2
    guard let cropped = cg.cropping(to: CGRect(x: cx, y: cy, width: cw, height: ch)),
          let data = cropped.dataProvider?.data as Data? else { t += 1.0/fps; continue }
    let bpr = cropped.bytesPerRow, bpp = cropped.bitsPerPixel/8
    var rs = 0, gs = 0, bs = 0, n = 0
    data.withUnsafeBytes { (p: UnsafeRawBufferPointer) in
        var y = 0
        while y < ch { var x = 0
            while x < cw {
                let o = y*bpr + x*bpp
                bs += Int(p[o]); gs += Int(p[o+1]); rs += Int(p[o+2]); n += 1
                x += 8 }
            y += 8 }
    }
    let name = String(format: "f%07.3f.png", t)
    let rep = NSBitmapImageRep(cgImage: cg)
    if let png = rep.representation(using: .png, properties: [:]) {
        try? png.write(to: URL(fileURLWithPath: "\(outdir)/\(name)"))
    }
    print(String(format: "%7.3f  r=%3d g=%3d b=%3d  %@", t, rs/max(n,1), gs/max(n,1), bs/max(n,1), name))
    t += 1.0/fps
}
