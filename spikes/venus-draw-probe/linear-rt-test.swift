// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// linear-rt-test: can Apple Silicon Metal RENDER (draw, not just clear/copy) into a
// buffer-backed LINEAR texture (newTextureWithDescriptor:offset:bytesPerRow:)?
//
// KosmicKrisp backs VK_IMAGE_TILING_LINEAR images with exactly such textures
// (mtl_new_texture_with_descriptor_linear); the limina KK zero-copy scanout path forces
// scanout images LINEAR so their memory can be host-pointer-imported IOSurface bytes.
// In-guest evidence: clears land in the linear target, draws are dropped (stencil-test
// red, desktop... renders?). This isolates the Metal capability with no VM/venus/zink.
//
// Cases: (a) draw, no DS attachment; (b) draw with D32S8 DS attachment + stencil test.
// Each prints CLEAR/D RAW pixel results read straight from the backing MTLBuffer.
//
// Build/run: swiftc -O linear-rt-test.swift -o linear-rt-test && ./linear-rt-test
import Metal
import IOSurface

let dev = MTLCreateSystemDefaultDevice()!
print("device=\(dev.name)")
let W = 64, H = 64, BPR = 256  // BGRA8, 64*4=256 (already aligned)

let shaderSrc = """
#include <metal_stdlib>
using namespace metal;
struct VOut { float4 pos [[position]]; };
vertex VOut vmain(uint vid [[vertex_id]]) {
    float2 p[3] = { float2(-1,-3), float2(-1,1), float2(3,1) };  // covers full screen
    VOut o; o.pos = float4(p[vid], 0, 1); return o;
}
fragment float4 fmain() { return float4(0, 1, 0, 1); }  // green
"""
let lib = try! dev.makeLibrary(source: shaderSrc, options: nil)

// The KosmicKrisp/limina provenance: the linear texture's MTLBuffer is a
// newBufferWithBytesNoCopy over IOSurfaceGetBaseAddress (host-pointer import of the
// scanout IOSurface). Plain device-allocated buffers render fine — this asks whether
// the bytesNoCopy-over-IOSurface variant also does, with and without a DS attachment.
func makeBuf(ioSurfaceBacked: Bool) -> (MTLBuffer, IOSurface?)? {
    if !ioSurfaceBacked {
        return (dev.makeBuffer(length: BPR * H, options: .storageModeShared)!, nil)
    }
    let props: [IOSurfacePropertyKey: Any] = [
        .width: W, .height: H, .bytesPerElement: 4, .bytesPerRow: BPR,
        .pixelFormat: UInt32(0x42475241),  // 'BGRA'
    ]
    guard let io = IOSurface(properties: props) else { return nil }
    let base = io.baseAddress
    let len = (io.allocationSize + 16383) & ~16383
    guard let buf = dev.makeBuffer(bytesNoCopy: base, length: len,
                                   options: .storageModeShared, deallocator: nil) else {
        print("bytesNoCopy buffer over IOSurface FAILED")
        return nil
    }
    return (buf, io)
}

func run(withDS: Bool, ioSurface: Bool = false) {
    guard let (buf, io) = makeBuf(ioSurfaceBacked: ioSurface) else { return }
    _ = io  // keep alive
    let td = MTLTextureDescriptor.texture2DDescriptor(
        pixelFormat: .bgra8Unorm, width: W, height: H, mipmapped: false)
    td.usage = [.renderTarget, .shaderRead]
    td.storageMode = .shared
    guard let tex = buf.makeTexture(descriptor: td, offset: 0, bytesPerRow: BPR) else {
        print("ds=\(withDS): linear texture creation FAILED (RenderTarget usage rejected)")
        return
    }

    let pd = MTLRenderPipelineDescriptor()
    pd.vertexFunction = lib.makeFunction(name: "vmain")
    pd.fragmentFunction = lib.makeFunction(name: "fmain")
    pd.colorAttachments[0].pixelFormat = .bgra8Unorm
    var dsTex: MTLTexture? = nil
    var dsState: MTLDepthStencilState? = nil
    if withDS {
        let dstd = MTLTextureDescriptor.texture2DDescriptor(
            pixelFormat: .depth32Float_stencil8, width: W, height: H, mipmapped: false)
        dstd.usage = .renderTarget
        dstd.storageMode = .private
        dsTex = dev.makeTexture(descriptor: dstd)
        pd.depthAttachmentPixelFormat = .depth32Float_stencil8
        pd.stencilAttachmentPixelFormat = .depth32Float_stencil8
        let dsd = MTLDepthStencilDescriptor()
        let s = MTLStencilDescriptor()
        s.stencilCompareFunction = .always
        dsd.frontFaceStencil = s
        dsd.backFaceStencil = s
        dsState = dev.makeDepthStencilState(descriptor: dsd)
    }
    let pso = try! dev.makeRenderPipelineState(descriptor: pd)

    let q = dev.makeCommandQueue()!
    let cb = q.makeCommandBuffer()!
    let rp = MTLRenderPassDescriptor()
    rp.colorAttachments[0].texture = tex
    rp.colorAttachments[0].loadAction = .clear
    rp.colorAttachments[0].clearColor = MTLClearColor(red: 1, green: 0, blue: 0, alpha: 1)
    rp.colorAttachments[0].storeAction = .store
    if let dsTex {
        rp.depthAttachment.texture = dsTex
        rp.depthAttachment.loadAction = .clear
        rp.depthAttachment.storeAction = .dontCare
        rp.stencilAttachment.texture = dsTex
        rp.stencilAttachment.loadAction = .clear
        rp.stencilAttachment.storeAction = .dontCare
    }
    let enc = cb.makeRenderCommandEncoder(descriptor: rp)!
    enc.setRenderPipelineState(pso)
    if let dsState { enc.setDepthStencilState(dsState) }
    enc.drawPrimitives(type: .triangle, vertexStart: 0, vertexCount: 3)
    enc.endEncoding()
    cb.commit()
    cb.waitUntilCompleted()
    if let err = cb.error { print("ds=\(withDS): command buffer error: \(err)") }

    let px = buf.contents().assumingMemoryBound(to: UInt8.self)
    let mid = (H / 2) * BPR + (W / 2) * 4
    let (b, g, r) = (px[mid], px[mid + 1], px[mid + 2])
    let verdict = g > 200 ? "DRAW LANDED (green)" : (r > 200 ? "DRAW DROPPED (clear-only red)" : "UNEXPECTED")
    print("ds=\(withDS) iosurface=\(ioSurface): center=(r:\(r) g:\(g) b:\(b)) -> \(verdict)")
}

run(withDS: false)
run(withDS: true)
run(withDS: false, ioSurface: true)
run(withDS: true, ioSurface: true)
