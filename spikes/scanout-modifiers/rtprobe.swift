// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// rtprobe — M15 wave-4 spike: what does the scanout buffer's backing type cost as a
// render target on Apple GPUs?
//
// Context: on KK the guest's scanout image is a *buffer-backed linear* MTLTexture over
// the IOSurface base address (virgl 0011 host-pointer import). The guest compositor's
// present blit draws into it. The candidate improvement is backing it with a proper
// IOSurface-backed texture (newTextureWithDescriptor:iosurface:) — the same thing a
// CAMetalLayer drawable is — which may render cheaper and would let the guest skip its
// shadow pass and render the whole scene directly into the scanout buffer.
//
// Measures GPU time (cb.gpuEndTime - cb.gpuStartTime) for two workloads into four
// 3840x2160 BGRA8 targets:
//   private-tiled     — MTLStorageModePrivate texture (the compositor-shadow baseline)
//   shared-tiled      — MTLStorageModeShared plain texture
//   buffer-linear     — texture created on an MTLBuffer (today's vkr scanout backing)
//   iosurface         — texture created from an IOSurface (the candidate)
// Workloads:
//   blit  — 1 fullscreen textured draw (today's present blit)
//   scene — 60 blended layered draws sampling a source texture (a compositor frame)
//
// Build: swiftc -O rtprobe.swift -o rtprobe
// Run:   ./rtprobe [frames]   (default 300)

import Metal
import IOSurface
import Foundation

let W = 3840, H = 2160
let frames = CommandLine.arguments.count > 1 ? Int(CommandLine.arguments[1]) ?? 300 : 300

guard let dev = MTLCreateSystemDefaultDevice() else { fatalError("no Metal device") }
print("device: \(dev.name)")

let shaders = """
#include <metal_stdlib>
using namespace metal;
struct VOut { float4 pos [[position]]; float2 uv; };
vertex VOut vs(uint vid [[vertex_id]], constant float4 &rect [[buffer(0)]]) {
    // rect = (x0,y0,x1,y1) in NDC; fullscreen triangle-strip quad scaled to rect
    float2 corners[4] = { float2(0,0), float2(1,0), float2(0,1), float2(1,1) };
    float2 c = corners[vid];
    VOut o;
    o.pos = float4(mix(rect.x, rect.z, c.x), mix(rect.y, rect.w, c.y), 0, 1);
    o.uv = c;
    return o;
}
fragment float4 fs(VOut in [[stage_in]], texture2d<float> src [[texture(0)]],
                   constant float4 &tint [[buffer(0)]]) {
    constexpr sampler s(filter::linear);
    float4 t = src.sample(s, in.uv);
    return float4(t.rgb * tint.rgb, tint.a);
}
"""
let lib = try dev.makeLibrary(source: shaders, options: nil)

func pipeline(blend: Bool) -> MTLRenderPipelineState {
    let d = MTLRenderPipelineDescriptor()
    d.vertexFunction = lib.makeFunction(name: "vs")
    d.fragmentFunction = lib.makeFunction(name: "fs")
    d.colorAttachments[0].pixelFormat = .bgra8Unorm
    if blend {
        let a = d.colorAttachments[0]!
        a.isBlendingEnabled = true
        a.sourceRGBBlendFactor = .sourceAlpha
        a.destinationRGBBlendFactor = .oneMinusSourceAlpha
        a.sourceAlphaBlendFactor = .one
        a.destinationAlphaBlendFactor = .oneMinusSourceAlpha
    }
    return try! dev.makeRenderPipelineState(descriptor: d)
}
let psoOpaque = pipeline(blend: false)
let psoBlend = pipeline(blend: true)

// Source texture to sample from (private, initialized via replaceRegion on a staging shared texture + blit)
let srcDesc = MTLTextureDescriptor.texture2DDescriptor(pixelFormat: .bgra8Unorm, width: W, height: H, mipmapped: false)
srcDesc.usage = [.shaderRead]
srcDesc.storageMode = .shared
let src = dev.makeTexture(descriptor: srcDesc)!
var seed: UInt32 = 12345
var pixels = [UInt32](repeating: 0, count: W * H)
for i in 0..<pixels.count { seed = seed &* 1664525 &+ 1013904223; pixels[i] = seed | 0xFF00_0000 }
pixels.withUnsafeBytes { src.replace(region: MTLRegionMake2D(0, 0, W, H), mipmapLevel: 0, withBytes: $0.baseAddress!, bytesPerRow: W * 4) }

// ---- targets ----
func makeTarget(_ kind: String) -> (String, MTLTexture)? {
    let d = MTLTextureDescriptor.texture2DDescriptor(pixelFormat: .bgra8Unorm, width: W, height: H, mipmapped: false)
    d.usage = [.renderTarget, .shaderRead]
    switch kind {
    case "private-tiled":
        d.storageMode = .private
        return dev.makeTexture(descriptor: d).map { (kind, $0) }
    case "shared-tiled":
        d.storageMode = .shared
        return dev.makeTexture(descriptor: d).map { (kind, $0) }
    case "buffer-linear":
        d.storageMode = .shared
        let align = dev.minimumLinearTextureAlignment(for: .bgra8Unorm)
        let bpr = ((W * 4) + align - 1) / align * align
        guard let buf = dev.makeBuffer(length: bpr * H, options: .storageModeShared) else { return nil }
        return buf.makeTexture(descriptor: d, offset: 0, bytesPerRow: bpr).map { (kind, $0) }
    case "iosurface":
        let props: [IOSurfacePropertyKey: Any] = [
            .width: W, .height: H, .bytesPerElement: 4,
            .pixelFormat: UInt32(0x42475241), // 'BGRA'
        ]
        guard let surf = IOSurface(properties: props) else { return nil }
        d.storageMode = .shared
        return dev.makeTexture(descriptor: d, iosurface: unsafeDowncast(surf, to: IOSurfaceRef.self), plane: 0).map { (kind, $0) }
    default: return nil
    }
}

let queue = dev.makeCommandQueue()!

func run(_ name: String, target: MTLTexture, draws: Int, blend: Bool) -> (Double, Double, Double) {
    var times: [Double] = []
    times.reserveCapacity(frames)
    // deterministic pseudo-random rects/tints per draw index (same for every target)
    var rects: [SIMD4<Float>] = []
    var tints: [SIMD4<Float>] = []
    var s: UInt32 = 42
    func rnd() -> Float { s = s &* 1664525 &+ 1013904223; return Float(s >> 8) / Float(1 << 24) }
    for i in 0..<draws {
        if i == 0 || draws == 1 {
            rects.append(SIMD4(-1, -1, 1, 1)) // first/only draw fullscreen
        } else {
            let x = rnd() * 1.2 - 1.1, y = rnd() * 1.2 - 1.1
            let w = 0.3 + rnd() * 1.4, h = 0.3 + rnd() * 1.4
            rects.append(SIMD4(x, y, min(x + w, 1), min(y + h, 1)))
        }
        tints.append(SIMD4(0.5 + rnd() * 0.5, 0.5 + rnd() * 0.5, 0.5 + rnd() * 0.5, blend ? 0.6 : 1.0))
    }
    for f in 0..<(frames + 20) {
        let cb = queue.makeCommandBuffer()!
        let rp = MTLRenderPassDescriptor()
        rp.colorAttachments[0].texture = target
        rp.colorAttachments[0].loadAction = .clear
        rp.colorAttachments[0].clearColor = MTLClearColor(red: 0, green: 0, blue: 0, alpha: 1)
        rp.colorAttachments[0].storeAction = .store
        guard let enc = cb.makeRenderCommandEncoder(descriptor: rp) else {
            print("  \(name): FAILED to make render encoder"); return (-1, -1, -1)
        }
        for i in 0..<draws {
            enc.setRenderPipelineState(i == 0 ? psoOpaque : (blend ? psoBlend : psoOpaque))
            var r = rects[i]; var t = tints[i]
            enc.setVertexBytes(&r, length: 16, index: 0)
            enc.setFragmentBytes(&t, length: 16, index: 0)
            enc.setFragmentTexture(src, index: 0)
            enc.drawPrimitives(type: .triangleStrip, vertexStart: 0, vertexCount: 4)
        }
        enc.endEncoding()
        cb.commit()
        cb.waitUntilCompleted()
        if cb.status == .error { print("  \(name): cb ERROR \(cb.error.map(String.init(describing:)) ?? "")"); return (-1, -1, -1) }
        if f >= 20 { times.append((cb.gpuEndTime - cb.gpuStartTime) * 1000) }
    }
    times.sort()
    return (times[times.count / 2], times[times.count * 9 / 10], times[times.count - 1])
}

let kinds = ["private-tiled", "shared-tiled", "buffer-linear", "iosurface"]
for (wname, draws, blend) in [("blit", 1, false), ("scene", 60, true)] {
    print("\n== workload: \(wname) (\(draws) draw(s)\(blend ? ", blended" : "")) — \(frames) frames, ms p50 / p90 / max ==")
    for k in kinds {
        guard let (name, tex) = makeTarget(k) else { print("  \(k): CREATE FAILED"); continue }
        let (p50, p90, mx) = run(name, target: tex, draws: draws, blend: blend)
        if p50 >= 0 { print(String(format: "  %-14s %7.3f / %7.3f / %7.3f", (name as NSString).utf8String!, p50, p90, mx)) }
    }
}
