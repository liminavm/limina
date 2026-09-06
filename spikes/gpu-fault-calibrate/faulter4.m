// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva
//
// The same deliberate fault as faulter.swift, but through the API KosmicKrisp
// actually uses: an MTL4 command queue, an argument table bound by raw address,
// and an explicit residency set. The classic-Metal version does NOT fault -- a
// shader load from an unmapped address returns zero there -- and the two paths
// differ in exactly the way that could explain it, since MTL4 makes residency
// the program's job. So the classic result decides nothing until this one runs.

#import <Metal/Metal.h>
#import <Foundation/Foundation.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

int main(int argc, char **argv)
{
   @autoreleasepool {
      if (argc < 3) {
         fprintf(stderr, "usage: faulter4 <load|texload> <hex-address|self>\n");
         return 2;
      }
      const char *mode = argv[1];
      bool self_test = strcmp(argv[2], "self") == 0;
      uint64_t addr = self_test ? 0 : strtoull(argv[2], NULL, 16);

      id<MTLDevice> dev = MTLCreateSystemDefaultDevice();
      NSError *err = nil;

      NSString *src = [NSString stringWithContentsOfFile:@"fault.metal"
                                                encoding:NSUTF8StringEncoding error:&err];
      if (src == nil) { fprintf(stderr, "cannot read fault.metal next to cwd\n"); return 2; }

      id<MTL4Compiler> comp = [dev newCompilerWithDescriptor:[MTL4CompilerDescriptor new] error:&err];
      MTLCompileOptions *opts = [MTLCompileOptions new];
      opts.languageVersion = MTLLanguageVersion3_2;
      MTL4LibraryDescriptor *ld = [MTL4LibraryDescriptor new];
      ld.source = src;
      ld.options = opts;
      id<MTLLibrary> lib = [comp newLibraryWithDescriptor:ld error:&err];
      if (lib == nil) { fprintf(stderr, "library: %s\n", err.localizedDescription.UTF8String); return 1; }

      MTL4LibraryFunctionDescriptor *fd = [MTL4LibraryFunctionDescriptor new];
      fd.name = [NSString stringWithUTF8String:mode];
      fd.library = lib;
      MTL4ComputePipelineDescriptor *pd = [MTL4ComputePipelineDescriptor new];
      pd.computeFunctionDescriptor = fd;
      id<MTLComputePipelineState> pso = [comp newComputePipelineStateWithDescriptor:pd
                                                              compilerTaskOptions:nil error:&err];
      if (pso == nil) { fprintf(stderr, "pipeline: %s\n", err.localizedDescription.UTF8String); return 1; }

      id<MTLBuffer> cfg = [dev newBufferWithLength:16 options:MTLResourceStorageModeShared];
      id<MTLBuffer> out = [dev newBufferWithLength:4096 options:MTLResourceStorageModeShared];
      uint64_t *c = (uint64_t *)cfg.contents;
      c[0] = self_test ? cfg.gpuAddress + 8 : addr;
      c[1] = self_test ? 0xcafebabeull : addr;
      printf("cfg gpuAddress 0x%llx, target 0x%llx\n", (unsigned long long)cfg.gpuAddress,
             (unsigned long long)c[0]);

      /* Exactly KK's shape: the two buffers are resident, and the address the shader
       * dereferences is NOT -- it is a bare number in an argument table slot. */
      MTLResidencySetDescriptor *rsd = [MTLResidencySetDescriptor new];
      id<MTLResidencySet> rs = [dev newResidencySetWithDescriptor:rsd error:&err];
      [rs addAllocation:cfg];
      [rs addAllocation:out];
      [rs commit];
      [rs requestResidency];

      MTL4ArgumentTableDescriptor *atd = [MTL4ArgumentTableDescriptor new];
      atd.maxBufferBindCount = 2;
      id<MTL4ArgumentTable> table = [dev newArgumentTableWithDescriptor:atd error:&err];
      [table setAddress:cfg.gpuAddress atIndex:0];
      [table setAddress:out.gpuAddress atIndex:1];

      id<MTL4CommandQueue> q = [dev newMTL4CommandQueue];
      [q addResidencySet:rs];
      id<MTL4CommandAllocator> alloc = [dev newCommandAllocator];
      id<MTL4CommandBuffer> cb = [dev newCommandBuffer];
      [cb beginCommandBufferWithAllocator:alloc];
      id<MTL4ComputeCommandEncoder> enc = [cb computeCommandEncoder];
      [enc setArgumentTable:table];
      [enc setComputePipelineState:pso];
      [enc dispatchThreads:MTLSizeMake(64, 1, 1) threadsPerThreadgroup:MTLSizeMake(64, 1, 1)];
      [enc endEncoding];
      [cb endCommandBuffer];

      __block bool done = false;
      __block NSError *feedback_err = nil;
      MTL4CommitOptions *co = [MTL4CommitOptions new];
      [co addFeedbackHandler:^(id<MTL4CommitFeedback> fb) {
         feedback_err = fb.error;
         done = true;
      }];
      printf("pid %d: dispatching %s against 0x%llx via MTL4\n", getpid(), mode,
             (unsigned long long)c[0]);
      id<MTL4CommandBuffer> bufs[1] = { cb };
      [q commit:bufs count:1 options:co];

      for (int i = 0; i < 2000 && !done; i++)
         usleep(5000);

      if (!done)
         printf("no completion feedback after 10s\n");
      else if (feedback_err)
         printf("commit error: %s\n", feedback_err.description.UTF8String);
      else
         printf("completed with no error -- no fault taken\n");

      uint32_t *r = (uint32_t *)out.contents;
      printf("out[0..3] = %u %u %u %u\n", r[0], r[1], r[2], r[3]);
      return 0;
   }
}
