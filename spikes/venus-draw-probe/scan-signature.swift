// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Flag frames whose center-region mean color falls in a target band (e.g. the
// desktop wallpaper showing through a window that should be opaque) — absolute
// matching, no neighbor agreement, so it catches artifacts AT scene transitions
// where scan-anomalies.swift is blind (its neighbors-agree precondition fails).
// usage: swift scan-signature.swift <movie> <rMin> <rMax> <gMin> <gMax> <bMin> <bMax>
import AVFoundation
import AppKit

let args = CommandLine.arguments
guard args.count == 8 else {
    print("usage: <movie> <rMin> <rMax> <gMin> <gMax> <bMin> <bMax>")
    exit(1)
}
let url = URL(fileURLWithPath: args[1])
let rMin = Double(args[2])!, rMax = Double(args[3])!
let gMin = Double(args[4])!, gMax = Double(args[5])!
let bMin = Double(args[6])!, bMax = Double(args[7])!

let asset = AVAsset(url: url)
guard let track = asset.tracks(withMediaType: .video).first else { exit(1) }
let duration = CMTimeGetSeconds(asset.duration)
let reader = try! AVAssetReader(asset: asset)
let settings: [String: Any] = [kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_32BGRA]
let output = AVAssetReaderTrackOutput(track: track, outputSettings: settings)
reader.add(output)
reader.startReading()

func stats(_ pb: CVPixelBuffer) -> (Double, Double, Double) {
    CVPixelBufferLockBaseAddress(pb, .readOnly)
    defer { CVPixelBufferUnlockBaseAddress(pb, .readOnly) }
    let w = CVPixelBufferGetWidth(pb), h = CVPixelBufferGetHeight(pb)
    let bpr = CVPixelBufferGetBytesPerRow(pb)
    let base = CVPixelBufferGetBaseAddress(pb)!.assumingMemoryBound(to: UInt8.self)
    let cw = Int(Double(w) * 0.6), ch = Int(Double(h) * 0.6)
    let cx = (w - cw) / 2, cy = (h - ch) / 2
    var rs = 0.0, gs = 0.0, bs = 0.0, n = 0.0
    var y = cy
    while y < cy + ch {
        var x = cx
        while x < cx + cw {
            let o = y * bpr + x * 4
            bs += Double(base[o]); gs += Double(base[o + 1]); rs += Double(base[o + 2]); n += 1
            x += 16
        }
        y += 16
    }
    return (rs / n, gs / n, bs / n)
}

var nframes = 0
var hits = 0
while let sb = output.copyNextSampleBuffer() {
    guard let pb = CMSampleBufferGetImageBuffer(sb) else { continue }
    let t = CMTimeGetSeconds(CMSampleBufferGetPresentationTimeStamp(sb))
    let (r, g, b) = stats(pb)
    nframes += 1
    if r >= rMin && r <= rMax && g >= gMin && g <= gMax && b >= bMin && b <= bMax {
        hits += 1
        print(String(format: "SIGNATURE t=%8.3f  rgb=(%.0f,%.0f,%.0f)", t, r, g, b))
    }
}
print(String(format: "scanned %d frames over %.1fs, %d signature hits", nframes, duration, hits))
