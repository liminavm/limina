// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// blitlat.swift — how long does a full-frame IOSurface→IOSurface blit take on this GPU, while a
// guest renders on it? The copy-on-present design releases the guest's flush fence when the blit
// completes, so this is what the guest would wait.
//
// Blits a W×H BGRA IOSurface into another at ~60 Hz for N frames, each its own command buffer on
// one queue, and prints per blit:
//   queue_us  = GPU start - commit   (waiting for the GPU)
//   exec_us   = GPU end - GPU start  (the blit itself)
//   done_us   = completion handler - commit (what a waiter on the CPU sees)
// then percentiles.
//
// Build: swiftc -O blitlat.swift -o blitlat ; run: ./blitlat [frames] [width] [height]

import Foundation
import IOSurface
import Metal
import QuartzCore

let args = CommandLine.arguments
let frames = args.count > 1 ? Int(args[1])! : 600
let W = args.count > 2 ? Int(args[2])! : 2560
let H = args.count > 3 ? Int(args[3])! : 1440

func surface() -> IOSurface {
    IOSurface(properties: [
        .width: W, .height: H, .bytesPerElement: 4, .pixelFormat: UInt32(0x42_47_52_41),
    ])!
}

let dev = MTLCreateSystemDefaultDevice()!
let q = dev.makeCommandQueue()!
func texture(_ s: IOSurface) -> MTLTexture {
    let d = MTLTextureDescriptor.texture2DDescriptor(
        pixelFormat: .bgra8Unorm, width: W, height: H, mipmapped: false)
    d.usage = [.shaderRead, .shaderWrite]
    d.storageMode = .shared
    return dev.makeTexture(descriptor: d, iosurface: s, plane: 0)!
}
let src = texture(surface())
let dsts = (0..<3).map { _ in texture(surface()) }

var queueUs: [Double] = []
var execUs: [Double] = []
var doneUs: [Double] = []
let lock = NSLock()
let group = DispatchGroup()

for i in 0..<frames {
    let cb = q.makeCommandBuffer()!
    let blit = cb.makeBlitCommandEncoder()!
    blit.copy(from: src, to: dsts[i % 3])
    blit.endEncoding()
    group.enter()
    let commit = CACurrentMediaTime()
    cb.addCompletedHandler { cb in
        let done = CACurrentMediaTime()
        lock.lock()
        queueUs.append((cb.gpuStartTime - commit) * 1e6)
        execUs.append((cb.gpuEndTime - cb.gpuStartTime) * 1e6)
        doneUs.append((done - commit) * 1e6)
        lock.unlock()
        group.leave()
    }
    cb.commit()
    usleep(16_667)
}
group.wait()

func pct(_ v: [Double]) -> String {
    let s = v.sorted()
    let p = { (q: Double) in s[min(s.count - 1, Int(Double(s.count) * q))] }
    return String(format: "p10 %.0f p50 %.0f p90 %.0f p95 %.0f p99 %.0f max %.0f", p(0.1), p(0.5),
                  p(0.9), p(0.95), p(0.99), s.last!)
}
print("blits \(frames) of \(W)x\(H)")
print("queue_us " + pct(queueUs))
print("exec_us  " + pct(execUs))
print("done_us  " + pct(doneUs))
