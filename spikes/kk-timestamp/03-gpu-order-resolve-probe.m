// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

#import <Metal/Metal.h>
#import <Foundation/Foundation.h>

static id<MTLCounterSampleBuffer> mk_sb(id<MTLDevice> d, id<MTLCounterSet> ts, NSUInteger n){
  MTLCounterSampleBufferDescriptor *sbd = [MTLCounterSampleBufferDescriptor new];
  sbd.counterSet = ts; sbd.storageMode = MTLStorageModeShared; sbd.sampleCount = n;
  NSError *e=nil; id<MTLCounterSampleBuffer> sb=[d newCounterSampleBufferWithDescriptor:sbd error:&e];
  if(!sb) NSLog(@"sb alloc fail: %@", e);
  return sb;
}

int main(void){ @autoreleasepool {
  id<MTLDevice> d = MTLCreateSystemDefaultDevice();
  id<MTLCounterSet> ts=nil; for(id<MTLCounterSet> cs in d.counterSets) if([cs.name isEqualToString:@"timestamp"]) ts=cs;
  id<MTLCommandQueue> q=[d newCommandQueue];
  id<MTLBuffer> scratch=[d newBufferWithLength:1<<20 options:MTLResourceStorageModePrivate];

  // ---- TEST A: GPU-order resolve of a stage-boundary sample, fenced, SAME command buffer ----
  {
    id<MTLCounterSampleBuffer> sb = mk_sb(d, ts, 2);
    id<MTLBuffer> dst = [d newBufferWithLength:64 options:MTLResourceStorageModeShared];
    memset(dst.contents, 0xEE, 64);
    MTLTimestamp c0,g0; [d sampleTimestamps:&c0 gpuTimestamp:&g0];
    id<MTLCommandBuffer> cb=[q commandBuffer];
    id<MTLFence> f=[d newFence];
    // enc1: sampling blit encoder (start-of-encoder sample -> slot 0), with real work
    MTLBlitPassDescriptor *bpd=[MTLBlitPassDescriptor new];
    bpd.sampleBufferAttachments[0].sampleBuffer=sb;
    bpd.sampleBufferAttachments[0].startOfEncoderSampleIndex=0;
    bpd.sampleBufferAttachments[0].endOfEncoderSampleIndex=MTLCounterDontSample;
    id<MTLBlitCommandEncoder> e1=[cb blitCommandEncoderWithDescriptor:bpd];
    [e1 fillBuffer:scratch range:NSMakeRange(0,1<<20) value:3];
    [e1 updateFence:f];
    [e1 endEncoding];
    // enc2: waits fence, GPU-resolves slot 0 into dst
    id<MTLBlitCommandEncoder> e2=[cb blitCommandEncoder];
    [e2 waitForFence:f];
    [e2 resolveCounters:sb inRange:NSMakeRange(0,1) destinationBuffer:dst destinationOffset:0];
    [e2 endEncoding];
    [cb commit]; [cb waitUntilCompleted];
    MTLTimestamp c1,g1; [d sampleTimestamps:&c1 gpuTimestamp:&g1];
    uint64_t gpu_resolved=((const uint64_t*)dst.contents)[0];
    NSData *cpu=[sb resolveCounterRange:NSMakeRange(0,1)];
    uint64_t cpu_resolved=((const MTLCounterResultTimestamp*)cpu.bytes)[0].timestamp;
    NSLog(@"[A] cpu window   %llu .. %llu", c0, c1);
    NSLog(@"[A] GPU-resolved %llu  (in window? %d)", gpu_resolved, (gpu_resolved>=c0&&gpu_resolved<=c1));
    NSLog(@"[A] CPU-resolved %llu  (match GPU? %d)", cpu_resolved, gpu_resolved==cpu_resolved);
  }

  // ---- TEST B: EMPTY sampling encoder (no blit commands) — is the start sample elided? ----
  {
    id<MTLCounterSampleBuffer> sb = mk_sb(d, ts, 1);
    id<MTLBuffer> dst=[d newBufferWithLength:64 options:MTLResourceStorageModeShared];
    memset(dst.contents,0,64);
    MTLTimestamp c0,g0;[d sampleTimestamps:&c0 gpuTimestamp:&g0];
    id<MTLCommandBuffer> cb=[q commandBuffer];
    id<MTLFence> f=[d newFence];
    MTLBlitPassDescriptor *bpd=[MTLBlitPassDescriptor new];
    bpd.sampleBufferAttachments[0].sampleBuffer=sb;
    bpd.sampleBufferAttachments[0].startOfEncoderSampleIndex=0;
    bpd.sampleBufferAttachments[0].endOfEncoderSampleIndex=MTLCounterDontSample;
    id<MTLBlitCommandEncoder> e1=[cb blitCommandEncoderWithDescriptor:bpd]; // NO commands
    [e1 updateFence:f];
    [e1 endEncoding];
    id<MTLBlitCommandEncoder> e2=[cb blitCommandEncoder];
    [e2 waitForFence:f];
    [e2 resolveCounters:sb inRange:NSMakeRange(0,1) destinationBuffer:dst destinationOffset:0];
    [e2 endEncoding];
    [cb commit]; [cb waitUntilCompleted];
    MTLTimestamp c1,g1;[d sampleTimestamps:&c1 gpuTimestamp:&g1];
    uint64_t v=((const uint64_t*)dst.contents)[0];
    NSLog(@"[B] empty-encoder start sample = %llu (in window %llu..%llu? %d)  [0 or garbage => elided]",
          v, c0, c1, (v>=c0&&v<=c1));
  }

  // ---- TEST C: two timestamps, monotonic across separate sampling encoders ----
  {
    id<MTLCounterSampleBuffer> sb = mk_sb(d, ts, 2);
    id<MTLBuffer> dst=[d newBufferWithLength:64 options:MTLResourceStorageModeShared];
    memset(dst.contents,0,64);
    id<MTLCommandBuffer> cb=[q commandBuffer];
    id<MTLFence> f0=[d newFence],f1=[d newFence];
    // ts0
    MTLBlitPassDescriptor *b0=[MTLBlitPassDescriptor new];
    b0.sampleBufferAttachments[0].sampleBuffer=sb; b0.sampleBufferAttachments[0].startOfEncoderSampleIndex=0; b0.sampleBufferAttachments[0].endOfEncoderSampleIndex=MTLCounterDontSample;
    id<MTLBlitCommandEncoder> e0=[cb blitCommandEncoderWithDescriptor:b0]; [e0 fillBuffer:scratch range:NSMakeRange(0,1<<20) value:1]; [e0 updateFence:f0]; [e0 endEncoding];
    // work between
    id<MTLBlitCommandEncoder> ew=[cb blitCommandEncoder]; [ew waitForFence:f0]; [ew fillBuffer:scratch range:NSMakeRange(0,1<<20) value:2]; [ew updateFence:f1]; [ew endEncoding];
    // ts1
    MTLBlitPassDescriptor *b1=[MTLBlitPassDescriptor new];
    b1.sampleBufferAttachments[0].sampleBuffer=sb; b1.sampleBufferAttachments[0].startOfEncoderSampleIndex=1; b1.sampleBufferAttachments[0].endOfEncoderSampleIndex=MTLCounterDontSample;
    id<MTLBlitCommandEncoder> e1=[cb blitCommandEncoderWithDescriptor:b1]; [e1 waitForFence:f1]; [e1 resolveCounters:sb inRange:NSMakeRange(0,2) destinationBuffer:dst destinationOffset:0]; [e1 endEncoding];
    [cb commit]; [cb waitUntilCompleted];
    uint64_t t0=((const uint64_t*)dst.contents)[0], t1=((const uint64_t*)dst.contents)[1];
    NSLog(@"[C] ts0=%llu ts1=%llu  monotonic(t1>t0)? %d  elapsed=%lld ns", t0, t1, t1>t0, (long long)(t1-t0));
  }
  return 0;
}}
