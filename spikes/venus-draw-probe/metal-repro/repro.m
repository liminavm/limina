// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// repro.m — standalone Metal reproduction attempt for the tier-2 "bottom-left-triangle-only" defect (#31).
//
// Everything here is replicated from the Xcode .gputrace of the live broken desktop (which REPRODUCES
// the defect on replay, so the command stream alone is sufficient):
//   - VS/FS: the zink-generated MSL extracted verbatim from the trace (vs.metal / fs.metal beside this file)
//   - vertex layout: cogl journal record, ONE interleaved buffer, 32-byte stride:
//       pos float3 @0, color uchar4norm @12, bytes 16..31 = never-written padding (filled with garbage here,
//       as zink's invalidate-fresh buffers are)
//   - bindings: two views of that buffer — Metal buffer index 30 (pos, offset 0) and 29 (color, offset 12),
//     both with DYNAMIC stride (MTLBufferLayoutStrideDynamic + setVertexBuffer:offset:attributeStride:)
//   - indices: uint16, the cogl rectangle pattern {0,1,2, 0,2,3} per quad, one draw, 3 quads (indexCount 18)
//   - pipeline: blending DISABLED, writeMask All, BGRA8Unorm color target, no depth attachment
//   - uniform buffer(1): 16 uints = column-major mat4; synthesized so gl_Position == the trace's table values
//     (w = -z = 50.37, quads on-screen)
//   - target: offscreen BGRA8 texture, clear transparent, store; readback + per-half coverage check
//
// Knobs (bisection menu once it reproduces — or fidelity suspects if it doesn't):
//   REPRO_STATIC_STRIDE=1   use static stride in the vertex descriptor (no dynamic-stride path)
//   REPRO_NEGVP=1           negative-height viewport (main-framebuffer style) instead of plain
//   REPRO_DEPTH=1           attach a depth attachment (depth test Always, no write)
//   REPRO_SEPARATE=1        put pos and color in two separate MTLBuffers instead of two views of one
//   REPRO_BIG=1             1280x800 target (default 900x120, dash-FBO-like)
//   REPRO_QUADS=n           number of quads (default 3)
//
// Verdict output: per quad, samples one pixel inside tri1 (bottom-left half) and one inside tri2
// (top-right half). Desktop defect = tri1 painted, tri2 NOT.

#import <Metal/Metal.h>
#import <Foundation/Foundation.h>

static NSString* readFile(NSString* path) {
    NSError* err = nil;
    NSString* s = [NSString stringWithContentsOfFile:path encoding:NSUTF8StringEncoding error:&err];
    if (!s) { fprintf(stderr, "FATAL: cannot read %s: %s\n", path.UTF8String, err.localizedDescription.UTF8String); exit(1); }
    return s;
}

int main(int argc, char** argv) {
    @autoreleasepool {
        bool staticStride = getenv("REPRO_STATIC_STRIDE");
        bool negVP        = getenv("REPRO_NEGVP");
        bool withDepth    = getenv("REPRO_DEPTH");
        bool separateBufs = getenv("REPRO_SEPARATE");
        bool big          = getenv("REPRO_BIG");
        int  nQuads       = getenv("REPRO_QUADS") ? atoi(getenv("REPRO_QUADS")) : 3;
        int W = big ? 1280 : 900, H = big ? 800 : 120;

        id<MTLDevice> dev = MTLCreateSystemDefaultDevice();
        if (!dev) { fprintf(stderr, "FATAL: no Metal device\n"); return 1; }
        fprintf(stderr, "device: %s  staticStride=%d negVP=%d depth=%d separate=%d %dx%d quads=%d\n",
                dev.name.UTF8String, staticStride, negVP, withDepth, separateBufs, W, H, nQuads);

        NSString* dir = [[NSString stringWithUTF8String:argv[0]] stringByDeletingLastPathComponent];
        NSError* err = nil;
        id<MTLLibrary> vsLib = [dev newLibraryWithSource:readFile([dir stringByAppendingPathComponent:@"vs.metal"]) options:nil error:&err];
        if (!vsLib) { fprintf(stderr, "FATAL: vs compile: %s\n", err.localizedDescription.UTF8String); return 1; }
        id<MTLLibrary> fsLib = [dev newLibraryWithSource:readFile([dir stringByAppendingPathComponent:@"fs.metal"]) options:nil error:&err];
        if (!fsLib) { fprintf(stderr, "FATAL: fs compile: %s\n", err.localizedDescription.UTF8String); return 1; }

        // ---- vertex data: cogl journal records, 32B stride, in PIXEL-ish model coords (z=-50.37) ----
        // Quad q: x in [40+q*120, 110+q*120], y in [20, 90] (y-down model space like cogl).
        // Record: pos float3 @0, rgba u8 @12, padding garbage @16.
        const float Z = -50.37f;
        size_t stride = 32;
        size_t vbLen = (size_t)nQuads * 4 * stride;
        uint8_t* rec = calloc(1, vbLen);
        srandom(7);
        for (size_t i = 0; i < vbLen; i++) rec[i] = (uint8_t)random();   // garbage everywhere first…
        for (int q = 0; q < nQuads; q++) {
            float x0 = 40.f + q * 120.f, x1 = x0 + 70.f, y0 = 20.f, y1 = 90.f;
            // cogl order: v0=TL v1=BL v2=BR v3=TR (y-down: TL=(x0,y0), BL=(x0,y1), BR=(x1,y1), TR=(x1,y0))
            float quad[4][2] = { {x0,y0}, {x0,y1}, {x1,y1}, {x1,y0} };
            for (int v = 0; v < 4; v++) {
                uint8_t* r = rec + ((size_t)q * 4 + v) * stride;
                float p[3] = { quad[v][0], quad[v][1], Z };
                memcpy(r, p, 12);                       // …then write ONLY pos
                uint8_t rgba[4] = { 56, 56, 59, 255 };  // and color (dock-slab gray, opaque)
                memcpy(r + 12, rgba, 4);                // bytes 16..31 stay garbage = unwritten padding
            }
        }
        id<MTLBuffer> vb = [dev newBufferWithBytes:rec length:vbLen options:MTLResourceStorageModeShared];
        free(rec);
        id<MTLBuffer> vbColor = vb; size_t posOff = 0, colOff = 12;
        if (separateBufs) {  // de-interleave into two tightly packed buffers
            uint8_t* pp = calloc(1, (size_t)nQuads * 4 * stride);
            uint8_t* cc = calloc(1, (size_t)nQuads * 4 * stride);
            const uint8_t* src = vb.contents;
            for (int v = 0; v < nQuads * 4; v++) {
                memcpy(pp + v * stride, src + v * stride, 12);
                memcpy(cc + v * stride, src + v * stride + 12, 4);
            }
            vb      = [dev newBufferWithBytes:pp length:(size_t)nQuads * 4 * stride options:MTLResourceStorageModeShared];
            vbColor = [dev newBufferWithBytes:cc length:(size_t)nQuads * 4 * stride options:MTLResourceStorageModeShared];
            posOff = 0; colOff = 0; free(pp); free(cc);
        }

        // ---- indices: uint16 cogl rectangle pattern ----
        uint16_t idx[256];
        for (int q = 0; q < nQuads; q++) {
            uint16_t b = (uint16_t)(q * 4);
            uint16_t pat[6] = { b, (uint16_t)(b+1), (uint16_t)(b+2), b, (uint16_t)(b+2), (uint16_t)(b+3) };
            memcpy(idx + q * 6, pat, sizeof pat);
        }
        id<MTLBuffer> ib = [dev newBufferWithBytes:idx length:(size_t)nQuads * 6 * 2 options:MTLResourceStorageModeShared];

        // ---- uniform buffer(1): column-major mat4 -> gl_Position = (x, y, 0, -z); w becomes 50.37 ----
        float M[16] = { 2.0f/W, 0, 0, 0,            // col0: x scale to NDC-ish… see below
                        0, 2.0f/H, 0, 0,            // col1
                        0, 0, 0, -1,                // col2: w = -z
                        -1, -1, 0, 0 };             // col3: translate
        // gl_Position = col0*x + col1*y + col2*z + col3*1 = (2x/W - 1, 2y/H - 1, 0, -z)
        // VS then does z=(z+w)*0.5 (in range) and y=-y (flip y-down model to NDC y-up).
        // NOTE: this puts NDC xy in [-1,1] BUT w=50.37 ≠ 1 divides them down — so pre-scale by w:
        for (int c = 0; c < 2; c++) for (int r = 0; r < 4; r++) M[c*4+r] *= 50.37f;
        M[12] *= 50.37f; M[13] *= 50.37f;
        id<MTLBuffer> ub = [dev newBufferWithBytes:M length:sizeof M options:MTLResourceStorageModeShared];

        // ---- pipeline ----
        MTLRenderPipelineDescriptor* pd = [MTLRenderPipelineDescriptor new];
        pd.vertexFunction   = [vsLib newFunctionWithName:@"main0"];
        pd.fragmentFunction = [fsLib newFunctionWithName:@"main0"];
        pd.colorAttachments[0].pixelFormat = MTLPixelFormatBGRA8Unorm;
        pd.colorAttachments[0].blendingEnabled = NO;                       // trace: Blending Disabled
        pd.colorAttachments[0].writeMask = MTLColorWriteMaskAll;           // trace: Write Mask All
        if (withDepth) pd.depthAttachmentPixelFormat = MTLPixelFormatDepth32Float;
        MTLVertexDescriptor* vd = [MTLVertexDescriptor vertexDescriptor];
        vd.attributes[0].format = MTLVertexFormatFloat3;            vd.attributes[0].offset = 0; vd.attributes[0].bufferIndex = 30;
        vd.attributes[1].format = MTLVertexFormatUChar4Normalized;  vd.attributes[1].offset = 0; vd.attributes[1].bufferIndex = 29;
        for (int b = 29; b <= 30; b++) {
            vd.layouts[b].stepFunction = MTLVertexStepFunctionPerVertex;
            vd.layouts[b].stepRate = 1;
            vd.layouts[b].stride = staticStride ? stride : MTLBufferLayoutStrideDynamic;
        }
        pd.vertexDescriptor = vd;
        id<MTLRenderPipelineState> pso = [dev newRenderPipelineStateWithDescriptor:pd error:&err];
        if (!pso) { fprintf(stderr, "FATAL: pso: %s\n", err.localizedDescription.UTF8String); return 1; }

        // ---- target ----
        MTLTextureDescriptor* td = [MTLTextureDescriptor texture2DDescriptorWithPixelFormat:MTLPixelFormatBGRA8Unorm width:W height:H mipmapped:NO];
        td.usage = MTLTextureUsageRenderTarget | MTLTextureUsageShaderRead;
        td.storageMode = MTLStorageModeShared;
        id<MTLTexture> tex = [dev newTextureWithDescriptor:td];
        id<MTLTexture> depthTex = nil;
        if (withDepth) {
            MTLTextureDescriptor* dd = [MTLTextureDescriptor texture2DDescriptorWithPixelFormat:MTLPixelFormatDepth32Float width:W height:H mipmapped:NO];
            dd.usage = MTLTextureUsageRenderTarget; dd.storageMode = MTLStorageModePrivate;
            depthTex = [dev newTextureWithDescriptor:dd];
        }

        MTLRenderPassDescriptor* rp = [MTLRenderPassDescriptor renderPassDescriptor];
        rp.colorAttachments[0].texture = tex;
        rp.colorAttachments[0].loadAction = MTLLoadActionClear;
        rp.colorAttachments[0].clearColor = MTLClearColorMake(0, 0, 0, 0);
        rp.colorAttachments[0].storeAction = MTLStoreActionStore;
        if (withDepth) {
            rp.depthAttachment.texture = depthTex;
            rp.depthAttachment.loadAction = MTLLoadActionClear;
            rp.depthAttachment.clearDepth = 1.0;
            rp.depthAttachment.storeAction = MTLStoreActionDontCare;
        }

        // Encoder-structure knobs — the trace shows the broken draws living in RESTARTED render-pass
        // segments (MoltenVK ends the encoder, runs a fan-conversion COMPUTE encoder, re-begins the
        // pass with loadAction=Load) with indexed-INDIRECT fan draws interleaved between the direct ones.
        bool multiDraw   = getenv("REPRO_MULTIDRAW");   // one draw per quad instead of one batched draw
        bool restart     = getenv("REPRO_RESTART");     // end/begin the pass (store/load) between draws
        bool computeMix  = getenv("REPRO_COMPUTE");     // dummy compute encoder between segments
        bool indirectMix = getenv("REPRO_INDIRECT_MIX");// indexed-indirect uint32 draw after each direct draw

        // Indirect-draw resources (mimic MoltenVK's fan path: uint32 temp indices + args buffer).
        // Draws a tiny quad at the far right so it doesn't overlap the probed quads.
        uint32_t fanIdx[6] = { 1, 2, 0, 2, 3, 0 };      // apex-last, like the fan conversion
        float fx0 = (float)W - 18.f, fx1 = (float)W - 4.f;
        uint8_t fanRec[4 * 32];
        memset(fanRec, 0, sizeof fanRec);
        float fq[4][2] = { {fx0,4.f}, {fx0,18.f}, {fx1,18.f}, {fx1,4.f} };
        for (int v = 0; v < 4; v++) {
            float p[3] = { fq[v][0], fq[v][1], Z };
            memcpy(fanRec + v * 32, p, 12);
            uint8_t rgba[4] = { 255, 0, 255, 255 };
            memcpy(fanRec + v * 32 + 12, rgba, 4);
        }
        id<MTLBuffer> fanVB  = [dev newBufferWithBytes:fanRec length:sizeof fanRec options:MTLResourceStorageModeShared];
        id<MTLBuffer> fanIB  = [dev newBufferWithBytes:fanIdx length:sizeof fanIdx options:MTLResourceStorageModeShared];
        MTLDrawIndexedPrimitivesIndirectArguments fanArgs = { 6, 1, 0, 0, 0 };
        id<MTLBuffer> fanArgB = [dev newBufferWithBytes:&fanArgs length:sizeof fanArgs options:MTLResourceStorageModeShared];

        id<MTLComputePipelineState> dummyCPS = nil;
        if (computeMix) {
            id<MTLLibrary> kLib = [dev newLibraryWithSource:
                @"#include <metal_stdlib>\nusing namespace metal;\nkernel void k(device uint* b [[buffer(0)]], uint t [[thread_position_in_grid]]) { b[t] = b[t] + 1; }"
                options:nil error:&err];
            dummyCPS = [dev newComputePipelineStateWithFunction:[kLib newFunctionWithName:@"k"] error:&err];
            if (!dummyCPS) { fprintf(stderr, "FATAL: compute pso: %s\n", err.localizedDescription.UTF8String); return 1; }
        }
        id<MTLBuffer> scratch = [dev newBufferWithLength:256 options:MTLResourceStorageModeShared];

        id<MTLCommandQueue> q = [dev newCommandQueue];
        id<MTLCommandBuffer> cb = [q commandBuffer];
        MTLDepthStencilDescriptor* dsd = [MTLDepthStencilDescriptor new];
        dsd.depthCompareFunction = MTLCompareFunctionAlways; dsd.depthWriteEnabled = NO;     // [LIMINA-DS] state
        id<MTLDepthStencilState> dss = withDepth ? [dev newDepthStencilStateWithDescriptor:dsd] : nil;
        MTLViewport vp = negVP ? (MTLViewport){0, (double)H, (double)W, -(double)H, 0, 1}
                               : (MTLViewport){0, 0, (double)W, (double)H, 0, 1};

        id<MTLRenderCommandEncoder> enc = nil;
        bool firstSegment = true;
        id<MTLRenderCommandEncoder> (^beginSegment)(void) = ^id<MTLRenderCommandEncoder>(void) {
            rp.colorAttachments[0].loadAction = firstSegment ? MTLLoadActionClear : MTLLoadActionLoad;
            rp.colorAttachments[0].storeAction = MTLStoreActionStore;
            id<MTLRenderCommandEncoder> e = [cb renderCommandEncoderWithDescriptor:rp];
            [e setRenderPipelineState:pso];
            if (dss) [e setDepthStencilState:dss];
            [e setCullMode:MTLCullModeNone];
            [e setViewport:vp];
            [e setScissorRect:(MTLScissorRect){0, 0, (NSUInteger)W, (NSUInteger)H}];
            if (staticStride) {
                [e setVertexBuffer:vb      offset:posOff atIndex:30];
                [e setVertexBuffer:vbColor offset:colOff atIndex:29];
            } else {
                [e setVertexBuffer:vb      offset:posOff attributeStride:stride atIndex:30];
                [e setVertexBuffer:vbColor offset:colOff attributeStride:stride atIndex:29];
            }
            [e setVertexBuffer:ub offset:0 atIndex:1];   // uniform_0_32
            return e;
        };
        enc = beginSegment(); firstSegment = false;

        int nDraws = multiDraw ? nQuads : 1;
        for (int d = 0; d < nDraws; d++) {
            NSUInteger idxCount  = multiDraw ? 6 : (NSUInteger)nQuads * 6;
            NSUInteger idxOffset = multiDraw ? (NSUInteger)d * 6 * 2 : 0;
            [enc drawIndexedPrimitives:MTLPrimitiveTypeTriangle
                            indexCount:idxCount
                             indexType:MTLIndexTypeUInt16
                           indexBuffer:ib
                     indexBufferOffset:idxOffset];
            if (indirectMix) {  // the fan draw MoltenVK interleaves: uint32, indexed-INDIRECT
                if (staticStride) [enc setVertexBuffer:fanVB offset:0 atIndex:30];
                else              [enc setVertexBuffer:fanVB offset:0 attributeStride:32 atIndex:30];
                if (staticStride) [enc setVertexBuffer:fanVB offset:12 atIndex:29];
                else              [enc setVertexBuffer:fanVB offset:12 attributeStride:32 atIndex:29];
                [enc drawIndexedPrimitives:MTLPrimitiveTypeTriangle
                                 indexType:MTLIndexTypeUInt32
                               indexBuffer:fanIB
                         indexBufferOffset:0
                            indirectBuffer:fanArgB
                      indirectBufferOffset:0];
                // restore the probe buffers
                if (staticStride) { [enc setVertexBuffer:vb offset:posOff atIndex:30]; [enc setVertexBuffer:vbColor offset:colOff atIndex:29]; }
                else { [enc setVertexBuffer:vb offset:posOff attributeStride:stride atIndex:30]; [enc setVertexBuffer:vbColor offset:colOff attributeStride:stride atIndex:29]; }
            }
            bool last = (d == nDraws - 1);
            if (restart && !last) {   // MoltenVK pass-restart: end (store) -> [compute] -> begin (load)
                [enc endEncoding];
                if (computeMix) {
                    id<MTLComputeCommandEncoder> ce = [cb computeCommandEncoder];
                    [ce setComputePipelineState:dummyCPS];
                    [ce setBuffer:scratch offset:0 atIndex:0];
                    [ce dispatchThreads:MTLSizeMake(32, 1, 1) threadsPerThreadgroup:MTLSizeMake(32, 1, 1)];
                    [ce endEncoding];
                }
                enc = beginSegment();
            }
        }
        [enc endEncoding];
        [cb commit];
        [cb waitUntilCompleted];

        // ---- readback + verdict ----
        size_t rowBytes = (size_t)W * 4;
        uint8_t* out = malloc(rowBytes * H);
        [tex getBytes:out bytesPerRow:rowBytes fromRegion:MTLRegionMake2D(0, 0, W, H) mipmapLevel:0];
        // NDC y-up flip: model y-down was flipped by the VS, so model TL lands at… sample both halves by
        // model coords mapped through the same transform the GPU used. Model (mx,my) -> ndc(2mx/W-1, -(2my/H-1))
        // -> pixel((ndc.x+1)/2*W, (1-ndc.y)/2*H) = (mx, my) for posVP. (negVP flips back: also (mx,my).)
        int fails = 0, passes = 0;
        for (int qd = 0; qd < nQuads; qd++) {
            float x0 = 40.f + qd * 120.f;
            // tri1 = {TL,BL,BR} = bottom-left half; sample near BL corner. tri2 = {TL,BR,TR} = top-right half; sample near TR.
            int t1x = (int)(x0 + 15), t1y = 75;   // inside bottom-left half
            int t2x = (int)(x0 + 55), t2y = 35;   // inside top-right half
            if (t2x >= W || t1x >= W) { fprintf(stderr, "quad %d: OFF-TARGET (W=%d), skipped\n", qd, W); continue; }
            const uint8_t* p1 = out + (size_t)t1y * rowBytes + (size_t)t1x * 4;
            const uint8_t* p2 = out + (size_t)t2y * rowBytes + (size_t)t2x * 4;
            bool tri1 = p1[3] != 0, tri2 = p2[3] != 0;
            fprintf(stderr, "quad %d: tri1(%d,%d)=%s [%u %u %u %u]  tri2(%d,%d)=%s [%u %u %u %u]\n",
                    qd, t1x, t1y, tri1 ? "PAINTED" : "EMPTY", p1[0],p1[1],p1[2],p1[3],
                    t2x, t2y, tri2 ? "PAINTED" : "EMPTY", p2[0],p2[1],p2[2],p2[3]);
            if (tri1 && !tri2) fails++;
            if (tri1 && tri2) passes++;
        }
        if (fails)            fprintf(stderr, "VERDICT: REPRODUCED — %d quad(s) bottom-left-triangle-only\n", fails);
        else if (passes == nQuads) fprintf(stderr, "VERDICT: clean — all quads whole (no repro)\n");
        else                  fprintf(stderr, "VERDICT: inconclusive — some quads entirely missing (transform off?)\n");
        free(out);
        return fails ? 42 : 0;
    }
}
