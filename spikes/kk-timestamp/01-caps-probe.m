// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

#import <Metal/Metal.h>
#import <Foundation/Foundation.h>
int main(void){ @autoreleasepool {
  id<MTLDevice> d = MTLCreateSystemDefaultDevice();
  NSLog(@"device: %@", d.name);
  NSLog(@"supportsCounterSampling AtStageBoundary   = %d", [d supportsCounterSampling:MTLCounterSamplingPointAtStageBoundary]);
  NSLog(@"supportsCounterSampling AtDrawBoundary     = %d", [d supportsCounterSampling:MTLCounterSamplingPointAtDrawBoundary]);
  NSLog(@"supportsCounterSampling AtDispatchBoundary = %d", [d supportsCounterSampling:MTLCounterSamplingPointAtDispatchBoundary]);
  NSLog(@"supportsCounterSampling AtBlitBoundary     = %d", [d supportsCounterSampling:MTLCounterSamplingPointAtBlitBoundary]);
  NSLog(@"supportsCounterSampling AtTileDispatchBoundary = %d", [d supportsCounterSampling:MTLCounterSamplingPointAtTileDispatchBoundary]);
  for (id<MTLCounterSet> cs in d.counterSets) {
    NSLog(@"counterSet: %@", cs.name);
    for (id<MTLCounter> c in cs.counters) NSLog(@"    counter: %@", c.name);
  }
  MTLTimestamp cpu=0,gpu=0; [d sampleTimestamps:&cpu gpuTimestamp:&gpu];
  NSLog(@"sampleTimestamps cpu=%llu gpu=%llu", (unsigned long long)cpu,(unsigned long long)gpu);
  return 0;
}}
