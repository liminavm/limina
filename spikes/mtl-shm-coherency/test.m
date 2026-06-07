// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Does a Metal StorageModeShared MTLBuffer created via newBufferWithBytesNoCopy
// over an shm_open()+mmap region (exactly vkr_mtl_shm_alloc) reflect GPU writes
// back to the CPU's view of the shm — with NO explicit synchronize? That is the
// host half of the venus host-visible-blob coherency question (#28). If YES, the
// host side is coherent and the black readback is a GUEST hv_vm_map issue.
#import <Metal/Metal.h>
#import <Foundation/Foundation.h>
#include <sys/mman.h>
#include <fcntl.h>
#include <unistd.h>
#include <stdio.h>
#include <string.h>

int main(void) {
  @autoreleasepool {
    const size_t SZ = 16384;            // one 16k host page
    const char *name = "/limina-coh-test";
    shm_unlink(name);
    int fd = shm_open(name, O_CREAT | O_EXCL | O_RDWR, 0600);
    if (fd < 0) { perror("shm_open"); return 1; }
    if (ftruncate(fd, SZ) != 0) { perror("ftruncate"); return 1; }
    void *p = mmap(NULL, SZ, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (p == MAP_FAILED) { perror("mmap"); return 1; }
    memset(p, 0x00, SZ);                 // CPU zeroes it (like a fresh staging buf)
    printf("before GPU: byte[0]=0x%02x byte[100]=0x%02x\n",
           ((unsigned char*)p)[0], ((unsigned char*)p)[100]);

    id<MTLDevice> dev = MTLCreateSystemDefaultDevice();
    printf("device=%s\n", dev.name.UTF8String);
    id<MTLBuffer> buf = [dev newBufferWithBytesNoCopy:p length:SZ
                                              options:MTLResourceStorageModeShared
                                          deallocator:nil];
    if (!buf) { printf("newBufferWithBytesNoCopy FAILED\n"); return 1; }

    // GPU fills the whole buffer with 0xAB via a blit (no shader needed).
    id<MTLCommandQueue> q = [dev newCommandQueue];
    id<MTLCommandBuffer> cb = [q commandBuffer];
    id<MTLBlitCommandEncoder> bl = [cb blitCommandEncoder];
    [bl fillBuffer:buf range:NSMakeRange(0, SZ) value:0xAB];
    [bl endEncoding];
    [cb commit];
    [cb waitUntilCompleted];
    printf("cmdbuf status=%ld error=%s\n", (long)cb.status,
           cb.error ? cb.error.localizedDescription.UTF8String : "none");

    // Read the CPU's mmap view — NO synchronize/invalidate. Coherent?
    unsigned char a = ((unsigned char*)p)[0], b = ((unsigned char*)p)[100], c = ((unsigned char*)p)[SZ-1];
    printf("after GPU (CPU mmap view): byte[0]=0x%02x byte[100]=0x%02x byte[last]=0x%02x\n", a, b, c);
    printf("RESULT: host CPU %s GPU writes to shm-backed Shared MTLBuffer\n",
           (a == 0xAB && b == 0xAB && c == 0xAB) ? "SEES" : "does NOT see");
    munmap(p, SZ); close(fd); shm_unlink(name);
    return (a == 0xAB) ? 0 : 2;
  }
}
