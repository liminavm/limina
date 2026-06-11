// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Exhaustive cut auditor: find every transition between the two scene clusters
// (by window-region mean), and flag any frame within +/-3 of a cut whose window
// region matches NEITHER scene (foreign/stale/backdrop content).
import AVFoundation
let args = CommandLine.arguments
let url = URL(fileURLWithPath: args[1])
let asset = AVAsset(url: url)
guard let track = asset.tracks(withMediaType: .video).first else { exit(1) }
let reader = try! AVAssetReader(asset: asset)
let settings: [String: Any] = [kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_32BGRA]
let output = AVAssetReaderTrackOutput(track: track, outputSettings: settings)
reader.add(output); reader.startReading()

func stats(_ pb: CVPixelBuffer) -> (Double, Double, Double) {
    CVPixelBufferLockBaseAddress(pb, .readOnly)
    defer { CVPixelBufferUnlockBaseAddress(pb, .readOnly) }
    let w = CVPixelBufferGetWidth(pb), h = CVPixelBufferGetHeight(pb)
    let bpr = CVPixelBufferGetBytesPerRow(pb)
    let base = CVPixelBufferGetBaseAddress(pb)!.assumingMemoryBound(to: UInt8.self)
    let cw = Int(Double(w)*0.30), ch = Int(Double(h)*0.30)
    let cx = (w-cw)/2, cy = (h-ch)/2
    var rs = 0.0, gs = 0.0, bs = 0.0, n = 0.0
    var y = cy
    while y < cy+ch { var x = cx
        while x < cx+cw { let o = y*bpr + x*4
            bs += Double(base[o]); gs += Double(base[o+1]); rs += Double(base[o+2]); n += 1
            x += 8 }
        y += 8 }
    return (rs/n, gs/n, bs/n)
}
// window-region clusters measured from extracted frames: cat (blue) vs crate (wood)
func dist(_ a: (Double,Double,Double), _ b: (Double,Double,Double)) -> Double {
    abs(a.0-b.0)+abs(a.1-b.1)+abs(a.2-b.2)
}
var frames: [(Double, (Double,Double,Double))] = []
while let sb = output.copyNextSampleBuffer() {
    guard let pb = CMSampleBufferGetImageBuffer(sb) else { continue }
    let t = CMTimeGetSeconds(CMSampleBufferGetPresentationTimeStamp(sb))
    frames.append((t, stats(pb)))
}
// derive the two clusters from the data: k-means-ish with 2 seeds from histogram extremes
var c1 = frames[10].1, c2 = frames[10].1
for f in frames { if dist(f.1, c1) > dist(c2, c1) { c2 = f.1 } }
for _ in 0..<6 {
    var s1 = (0.0,0.0,0.0), s2 = (0.0,0.0,0.0); var n1 = 0.0, n2 = 0.0
    for f in frames {
        if dist(f.1, c1) <= dist(f.1, c2) { s1 = (s1.0+f.1.0, s1.1+f.1.1, s1.2+f.1.2); n1 += 1 }
        else { s2 = (s2.0+f.1.0, s2.1+f.1.1, s2.2+f.1.2); n2 += 1 }
    }
    if n1 > 0 { c1 = (s1.0/n1, s1.1/n1, s1.2/n1) }
    if n2 > 0 { c2 = (s2.0/n2, s2.1/n2, s2.2/n2) }
}
print(String(format: "clusters: A=(%.0f,%.0f,%.0f) B=(%.0f,%.0f,%.0f)", c1.0,c1.1,c1.2, c2.0,c2.1,c2.2))
// classify; cuts = class changes; audit +/-3 frames for far-from-both
var cuts = 0, suspects = 0
var cls: [Int] = frames.map { dist($0.1, c1) <= dist($0.1, c2) ? 0 : 1 }
for i in 1..<frames.count where cls[i] != cls[i-1] {
    cuts += 1
    for j in max(0,i-3)...min(frames.count-1,i+3) {
        let d1 = dist(frames[j].1, c1), d2 = dist(frames[j].1, c2)
        if min(d1,d2) > 35 {
            suspects += 1
            print(String(format: "SUSPECT t=%8.3f rgb=(%.0f,%.0f,%.0f) dA=%.0f dB=%.0f (cut at t=%.3f)",
                frames[j].0, frames[j].1.0, frames[j].1.1, frames[j].1.2, d1, d2, frames[i].0))
        }
    }
}
print("cuts: \(cuts), suspect frames near cuts: \(suspects)")
// also: ANY frame far from both clusters anywhere
var far = 0
for f in frames where min(dist(f.1,c1), dist(f.1,c2)) > 50 {
    far += 1
    if far <= 20 { print(String(format: "FAR t=%8.3f rgb=(%.0f,%.0f,%.0f)", f.0, f.1.0, f.1.1, f.1.2)) }
}
print("frames far from both clusters anywhere: \(far)")
