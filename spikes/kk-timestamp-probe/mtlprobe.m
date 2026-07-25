// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/*
 * mtlprobe — what does this Apple GPU actually do with MTLCounterSampleBuffer?
 *
 * Companion to probe.c. probe.c showed that KosmicKrisp's timestamp queries resolve to zero on
 * M4 Pro and to real values on M1 Max, natively, with no VM in the picture. This drops to raw
 * Metal to find out which part of the sampling contract the failing GPU does not honour, so the
 * KK fix is the right one rather than a guess.
 *
 * KK samples like this (src/kosmickrisp/vulkan/kk_encoder.c:361-407):
 *   - a SHARED-storage MTLCounterSampleBuffer over the timestamp counter set
 *   - a blit encoder carrying sampleBufferAttachments[0], latching at its START boundary
 *   - a SECOND, distinct blit encoder that resolveCounters: into the query BO
 * so this probe walks that same shape, plus the variants it could be swapped for.
 *
 * Build/run on the host under test:
 *   clang -g -O0 -fobjc-arc -framework Metal -framework Foundation -o mtlprobe mtlprobe.m && ./mtlprobe
 */
#import <Foundation/Foundation.h>
#import <Metal/Metal.h>

static const char *
sampling_point_name(MTLCounterSamplingPoint p)
{
   switch (p) {
   case MTLCounterSamplingPointAtStageBoundary: return "AtStageBoundary";
   case MTLCounterSamplingPointAtDrawBoundary: return "AtDrawBoundary";
   case MTLCounterSamplingPointAtBlitBoundary: return "AtBlitBoundary";
   case MTLCounterSamplingPointAtDispatchBoundary: return "AtDispatchBoundary";
   case MTLCounterSamplingPointAtTileDispatchBoundary: return "AtTileDispatchBoundary";
   }
   return "?";
}

static id<MTLCounterSet>
timestamp_counter_set(id<MTLDevice> dev)
{
   for (id<MTLCounterSet> cs in dev.counterSets)
      if ([cs.name isEqualToString:MTLCommonCounterSetTimestamp])
         return cs;
   return nil;
}

static id<MTLCounterSampleBuffer>
make_sample_buffer(id<MTLDevice> dev, id<MTLCounterSet> cs, MTLStorageMode mode, NSUInteger n)
{
   MTLCounterSampleBufferDescriptor *d = [MTLCounterSampleBufferDescriptor new];
   d.counterSet = cs;
   d.storageMode = mode;
   d.sampleCount = n;
   NSError *err = nil;
   id<MTLCounterSampleBuffer> sb = [dev newCounterSampleBufferWithDescriptor:d error:&err];
   if (!sb)
      NSLog(@"    newCounterSampleBuffer(storage=%lu) failed: %@", (unsigned long)mode, err);
   return sb;
}

/* Latch two samples at the start/end boundary of one blit encoder — KK's shape.
 *
 * The encoder must carry REAL WORK. An empty blit encoder is elided before it ever reaches the
 * GPU, and then nothing samples: that reads back as [0, 0] even on hardware where the whole path
 * works, which is a false positive for the exact bug this probe is chasing. `scratch` supplies a
 * trivial copy so the encoder survives. */
static void
encode_sampling_pass(id<MTLCommandBuffer> cb,
                     id<MTLCounterSampleBuffer> sb,
                     id<MTLBuffer> scratch)
{
   MTLBlitPassDescriptor *bp = [MTLBlitPassDescriptor blitPassDescriptor];
   bp.sampleBufferAttachments[0].sampleBuffer = sb;
   bp.sampleBufferAttachments[0].startOfEncoderSampleIndex = 0;
   bp.sampleBufferAttachments[0].endOfEncoderSampleIndex = 1;
   id<MTLBlitCommandEncoder> enc = [cb blitCommandEncoderWithDescriptor:bp];
   [enc fillBuffer:scratch range:NSMakeRange(0, scratch.length) value:0x5a];
   [enc endEncoding];
}

static void
report(const char *label, uint64_t a, uint64_t b)
{
   const char *verdict = (a == 0 && b == 0)      ? "  <-- ZERO (the failure)"
                         : (a == UINT64_MAX)     ? "  <-- MTLCounterErrorValue"
                                                 : "  ok";
   printf("  %-34s [%llu, %llu]%s\n", label, (unsigned long long)a, (unsigned long long)b, verdict);
}

int
main(void)
{
   @autoreleasepool {
      id<MTLDevice> dev = MTLCreateSystemDefaultDevice();
      printf("device: %s\n\n", dev.name.UTF8String);

      printf("supportsCounterSampling:\n");
      for (MTLCounterSamplingPoint p = MTLCounterSamplingPointAtStageBoundary;
           p <= MTLCounterSamplingPointAtTileDispatchBoundary; p++)
         printf("  %-24s %s\n", sampling_point_name(p),
                [dev supportsCounterSampling:p] ? "YES" : "no");

      printf("\ncounter sets:\n");
      for (id<MTLCounterSet> cs in dev.counterSets)
         printf("  %s (%lu counters)\n", cs.name.UTF8String,
                (unsigned long)cs.counters.count);

      id<MTLCounterSet> ts = timestamp_counter_set(dev);
      if (!ts) {
         printf("\nno timestamp counter set — nothing further to test\n");
         return 0;
      }
      id<MTLCommandQueue> q = [dev newCommandQueue];
      id<MTLBuffer> scratch = [dev newBufferWithLength:4096
                                              options:MTLResourceStorageModeShared];

      printf("\nsampling a blit encoder's start/end boundary:\n");

      /* 1. SHARED storage + CPU resolveCounterRange:. */
      {
         id<MTLCounterSampleBuffer> sb = make_sample_buffer(dev, ts, MTLStorageModeShared, 2);
         if (sb) {
            id<MTLCommandBuffer> cb = [q commandBuffer];
            encode_sampling_pass(cb, sb, scratch);
            [cb commit];
            [cb waitUntilCompleted];
            NSData *data = [sb resolveCounterRange:NSMakeRange(0, 2)];
            const MTLCounterResultTimestamp *r = data.bytes;
            if (data.length >= 2 * sizeof(*r))
               report("shared  + CPU resolveCounterRange", r[0].timestamp, r[1].timestamp);
            else
               printf("  shared  + CPU resolveCounterRange   resolve returned %lu bytes\n",
                      (unsigned long)data.length);
         }
      }

      /* 2. SHARED storage + GPU resolveCounters: in a separate blit encoder — KK's exact path. */
      {
         id<MTLCounterSampleBuffer> sb = make_sample_buffer(dev, ts, MTLStorageModeShared, 2);
         id<MTLBuffer> dst = [dev newBufferWithLength:2 * sizeof(uint64_t)
                                              options:MTLResourceStorageModeShared];
         if (sb) {
            id<MTLCommandBuffer> cb = [q commandBuffer];
            encode_sampling_pass(cb, sb, scratch);
            id<MTLBlitCommandEncoder> res = [cb blitCommandEncoder];
            [res resolveCounters:sb
                         inRange:NSMakeRange(0, 2)
               destinationBuffer:dst
               destinationOffset:0];
            [res endEncoding];
            [cb commit];
            [cb waitUntilCompleted];
            const uint64_t *v = dst.contents;
            report("shared  + GPU resolveCounters (KK)", v[0], v[1]);
         }
      }

      /* 3. PRIVATE storage + GPU resolveCounters: — the documented shape for GPU-side resolve. */
      {
         id<MTLCounterSampleBuffer> sb = make_sample_buffer(dev, ts, MTLStorageModePrivate, 2);
         id<MTLBuffer> dst = [dev newBufferWithLength:2 * sizeof(uint64_t)
                                              options:MTLResourceStorageModeShared];
         if (sb) {
            id<MTLCommandBuffer> cb = [q commandBuffer];
            encode_sampling_pass(cb, sb, scratch);
            id<MTLBlitCommandEncoder> res = [cb blitCommandEncoder];
            [res resolveCounters:sb
                         inRange:NSMakeRange(0, 2)
               destinationBuffer:dst
               destinationOffset:0];
            [res endEncoding];
            [cb commit];
            [cb waitUntilCompleted];
            const uint64_t *v = dst.contents;
            report("private + GPU resolveCounters", v[0], v[1]);
         }
      }

      /* 4. SHARED sample buffer, GPU resolve into a PRIVATE destination, blitted back. Some
       * devices document the resolve destination as needing private storage. */
      {
         id<MTLCounterSampleBuffer> sb = make_sample_buffer(dev, ts, MTLStorageModeShared, 2);
         id<MTLBuffer> priv = [dev newBufferWithLength:2 * sizeof(uint64_t)
                                               options:MTLResourceStorageModePrivate];
         id<MTLBuffer> dst = [dev newBufferWithLength:2 * sizeof(uint64_t)
                                              options:MTLResourceStorageModeShared];
         if (sb) {
            id<MTLCommandBuffer> cb = [q commandBuffer];
            encode_sampling_pass(cb, sb, scratch);
            id<MTLBlitCommandEncoder> res = [cb blitCommandEncoder];
            [res resolveCounters:sb
                         inRange:NSMakeRange(0, 2)
               destinationBuffer:priv
               destinationOffset:0];
            [res copyFromBuffer:priv
                   sourceOffset:0
                       toBuffer:dst
              destinationOffset:0
                           size:2 * sizeof(uint64_t)];
            [res endEncoding];
            [cb commit];
            [cb waitUntilCompleted];
            const uint64_t *v = dst.contents;
            report("shared  + GPU resolve -> private", v[0], v[1]);
         }
      }

      /* 5. Resolve from a SEPARATE command buffer, committed after the sampling one completes.
       * KK resolves in a separate encoder of the SAME command buffer. */
      {
         id<MTLCounterSampleBuffer> sb = make_sample_buffer(dev, ts, MTLStorageModeShared, 2);
         id<MTLBuffer> dst = [dev newBufferWithLength:2 * sizeof(uint64_t)
                                              options:MTLResourceStorageModeShared];
         if (sb) {
            id<MTLCommandBuffer> cb = [q commandBuffer];
            encode_sampling_pass(cb, sb, scratch);
            [cb commit];
            [cb waitUntilCompleted];

            id<MTLCommandBuffer> cb2 = [q commandBuffer];
            id<MTLBlitCommandEncoder> res = [cb2 blitCommandEncoder];
            [res resolveCounters:sb
                         inRange:NSMakeRange(0, 2)
               destinationBuffer:dst
               destinationOffset:0];
            [res endEncoding];
            [cb2 commit];
            [cb2 waitUntilCompleted];
            const uint64_t *v = dst.contents;
            report("shared  + GPU resolve, separate cb", v[0], v[1]);
         }
      }

      /* 6. KK's literal shape: sampleCount = 1, start-of-encoder only, resolve range (0,1). */
      {
         id<MTLCounterSampleBuffer> sb = make_sample_buffer(dev, ts, MTLStorageModeShared, 1);
         id<MTLBuffer> dst = [dev newBufferWithLength:2 * sizeof(uint64_t)
                                              options:MTLResourceStorageModeShared];
         if (sb) {
            id<MTLCommandBuffer> cb = [q commandBuffer];
            MTLBlitPassDescriptor *bp = [MTLBlitPassDescriptor blitPassDescriptor];
            bp.sampleBufferAttachments[0].sampleBuffer = sb;
            bp.sampleBufferAttachments[0].startOfEncoderSampleIndex = 0;
            bp.sampleBufferAttachments[0].endOfEncoderSampleIndex = MTLCounterDontSample;
            id<MTLBlitCommandEncoder> enc = [cb blitCommandEncoderWithDescriptor:bp];
            [enc fillBuffer:scratch range:NSMakeRange(0, scratch.length) value:0x5a];
            [enc endEncoding];
            id<MTLBlitCommandEncoder> res = [cb blitCommandEncoder];
            [res resolveCounters:sb
                         inRange:NSMakeRange(0, 1)
               destinationBuffer:dst
               destinationOffset:0];
            [res endEncoding];
            [cb commit];
            [cb waitUntilCompleted];
            const uint64_t *v = dst.contents;
            report("KK shape (count=1, GPU resolve)", v[0], v[1]);

            NSData *data = [sb resolveCounterRange:NSMakeRange(0, 1)];
            const MTLCounterResultTimestamp *r = data.bytes;
            if (data.length >= sizeof(*r))
               report("KK shape, CPU resolveCounterRange", r[0].timestamp, r[0].timestamp);
         }
      }

      /* 7. The CPU/GPU timestamp pair, as a control: this is the clock itself, and the guest
       * already sees it working through VK_EXT_calibrated_timestamps. */
      {
         MTLTimestamp cpu = 0, gpu = 0;
         [dev sampleTimestamps:&cpu gpuTimestamp:&gpu];
         printf("\nsampleTimestamps: cpu=%llu gpu=%llu%s\n", (unsigned long long)cpu,
                (unsigned long long)gpu, gpu ? "" : "   <-- GPU clock reads zero too");
      }
   }
   return 0;
}
