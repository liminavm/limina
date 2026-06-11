// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Scan a screen recording for single-frame visual anomalies: frames whose center-region
// mean color jumps far from BOTH neighbors (flicker frames). Prints candidate timestamps.
// usage: swift scan-anomalies.swift <movie> [threshold]
import AVFoundation
import AppKit

let args = CommandLine.arguments
let url = URL(fileURLWithPath: args[1])
let threshold = args.count > 2 ? Double(args[2])! : 40.0

let asset = AVAsset(url: url)
guard let track = asset.tracks(withMediaType: .video).first else { exit(1) }
let duration = CMTimeGetSeconds(asset.duration)
let reader = try! AVAssetReader(asset: asset)
let settings: [String: Any] = [kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_32BGRA]
let output = AVAssetReaderTrackOutput(track: track, outputSettings: settings)
reader.add(output)
reader.startReading()

var prev: (t: Double, r: Double, g: Double, b: Double)? = nil
var pending: (t: Double, r: Double, g: Double, b: Double)? = nil
var nframes = 0

func stats(_ pb: CVPixelBuffer) -> (Double, Double, Double) {
    CVPixelBufferLockBaseAddress(pb, .readOnly)
    defer { CVPixelBufferUnlockBaseAddress(pb, .readOnly) }
    let w = CVPixelBufferGetWidth(pb), h = CVPixelBufferGetHeight(pb)
    let bpr = CVPixelBufferGetBytesPerRow(pb)
    let base = CVPixelBufferGetBaseAddress(pb)!.assumingMemoryBound(to: UInt8.self)
    let cw = Int(Double(w)*0.6), ch = Int(Double(h)*0.6)
    let cx = (w-cw)/2, cy = (h-ch)/2
    var rs = 0.0, gs = 0.0, bs = 0.0, n = 0.0
    var y = cy
    while y < cy+ch {
        var x = cx
        while x < cx+cw {
            let o = y*bpr + x*4
            bs += Double(base[o]); gs += Double(base[o+1]); rs += Double(base[o+2]); n += 1
            x += 16
        }
        y += 16
    }
    return (rs/n, gs/n, bs/n)
}

while let sb = output.copyNextSampleBuffer() {
    guard let pb = CMSampleBufferGetImageBuffer(sb) else { continue }
    let t = CMTimeGetSeconds(CMSampleBufferGetPresentationTimeStamp(sb))
    let (r, g, b) = stats(pb)
    nframes += 1
    if let p = pending, let pr = prev {
        // pending is an anomaly candidate iff it differs strongly from BOTH neighbors
        // and the neighbors agree with each other.
        let dPrev = abs(p.r-pr.r) + abs(p.g-pr.g) + abs(p.b-pr.b)
        let dNext = abs(p.r-r) + abs(p.g-g) + abs(p.b-b)
        let dSides = abs(pr.r-r) + abs(pr.g-g) + abs(pr.b-b)
        if dPrev > threshold && dNext > threshold && dSides < threshold/2 {
            print(String(format: "ANOMALY t=%8.3f  rgb=(%.0f,%.0f,%.0f) vs neighbors (%.0f,%.0f,%.0f)",
                         p.t, p.r, p.g, p.b, pr.r, pr.g, pr.b))
        }
    }
    prev = pending
    pending = (t, r, g, b)
}
print(String(format: "scanned %d frames over %.1fs", nframes, duration))
