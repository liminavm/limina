// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

#import <Metal/Metal.h>
#import <Foundation/Foundation.h>
int main(void){ @autoreleasepool {
  id<MTLDevice> d = MTLCreateSystemDefaultDevice();
  id<MTLCounterSet> tsSet = nil;
  for (id<MTLCounterSet> cs in d.counterSets) if ([cs.name isEqualToString:@"timestamp"]) tsSet = cs;
  if (!tsSet){ NSLog(@"no timestamp counter set"); return 1; }

  MTLCounterSampleBufferDescriptor *sbd = [MTLCounterSampleBufferDescriptor new];
  sbd.counterSet = tsSet; sbd.storageMode = MTLStorageModeShared; sbd.sampleCount = 2;
  NSError *err = nil;
  id<MTLCounterSampleBuffer> sb = [d newCounterSampleBufferWithDescriptor:sbd error:&err];
  if (!sb){ NSLog(@"sample buffer failed: %@", err); return 1; }

  MTLTimestamp cpu0=0,gpu0=0; [d sampleTimestamps:&cpu0 gpuTimestamp:&gpu0];

  id<MTLCommandQueue> q = [d newCommandQueue];
  id<MTLCommandBuffer> cb = [q commandBuffer];
  // Blit pass with stage-boundary sampling: sample at start and end of the encoder.
  MTLBlitPassDescriptor *bpd = [MTLBlitPassDescriptor new];
  bpd.sampleBufferAttachments[0].sampleBuffer = sb;
  bpd.sampleBufferAttachments[0].startOfEncoderSampleIndex = 0;
  bpd.sampleBufferAttachments[0].endOfEncoderSampleIndex = 1;
  id<MTLBlitCommandEncoder> enc = [cb blitCommandEncoderWithDescriptor:bpd];
  // tiny real work so start!=end
  id<MTLBuffer> b = [d newBufferWithLength:1<<20 options:MTLResourceStorageModePrivate];
  [enc fillBuffer:b range:NSMakeRange(0,1<<20) value:7];
  [enc endEncoding];
  [cb commit]; [cb waitUntilCompleted];

  MTLTimestamp cpu1=0,gpu1=0; [d sampleTimestamps:&cpu1 gpuTimestamp:&gpu1];

  NSData *nd = [sb resolveCounterRange:NSMakeRange(0,2)];
  const MTLCounterResultTimestamp *r = (const MTLCounterResultTimestamp *)nd.bytes;
  NSLog(@"cpu window : %llu .. %llu (dur %llu ns)", cpu0, cpu1, cpu1-cpu0);
  NSLog(@"sample start: %llu", r[0].timestamp);
  NSLog(@"sample end  : %llu", r[1].timestamp);
  NSLog(@"encoder dur : %lld ns", (long long)(r[1].timestamp - r[0].timestamp));
  NSLog(@"start in cpu window? %d   end in cpu window? %d",
        (r[0].timestamp>=cpu0 && r[0].timestamp<=cpu1),
        (r[1].timestamp>=cpu0 && r[1].timestamp<=cpu1));
  NSLog(@"cb GPUStartTime=%f GPUEndTime=%f (dur %f ns)", cb.GPUStartTime, cb.GPUEndTime, (cb.GPUEndTime-cb.GPUStartTime)*1e9);
  return 0;
}}
