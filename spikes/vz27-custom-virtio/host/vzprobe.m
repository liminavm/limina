// vzprobe: host half of the macOS 27 Virtualization.framework custom-virtio spike.
//
// Boots a throwaway initramfs guest (../guest, PID 1) under VZ with:
//   - Apple's virtio console #0 -> stdout (the guest's log and kernel console, hvc0);
//   - Apple's virtio console #1 -> a pipe we echo with XOR 0x20 (the "apple" baseline);
//   - a VZCustomVirtioDevice speaking virtio-console (ID 3, no MULTIPORT) that echoes
//     verbatim, carrying one virtio shared-memory region (ID 1) for the mapping tests;
//   - Apple's vsock, port 5000 = the guest's command channel (see guest/src/main.rs).
//
// usage: vzprobe <kernel Image> <initramfs.cpio> [extra cmdline]

#import <Foundation/Foundation.h>
#import <Virtualization/Virtualization.h>
#import <Metal/Metal.h>
#import <IOSurface/IOSurface.h>
#import <libproc.h>
#import <mach/mach_time.h>
#import <sys/mman.h>

static uint64_t now_ns(void) {
    static mach_timebase_info_data_t tb;
    if (!tb.denom) mach_timebase_info(&tb);
    return mach_absolute_time() * tb.numer / tb.denom;
}

static void say(NSString *fmt, ...) NS_FORMAT_FUNCTION(1, 2);
static void say(NSString *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    NSString *s = [[NSString alloc] initWithFormat:fmt arguments:ap];
    va_end(ap);
    fprintf(stdout, "[vzprobe] %s\n", s.UTF8String);
    fflush(stdout);
}

static double footprint_mib(pid_t pid) {
    struct rusage_info_v4 ri;
    if (pid <= 0 || proc_pid_rusage(pid, RUSAGE_INFO_V4, (rusage_info_t *)&ri) != 0) return -1;
    return ri.ri_phys_footprint / 1048576.0;
}

static NSSet<NSNumber *> *vm_service_pids(void) {
    NSMutableSet *out = [NSMutableSet set];
    int n = proc_listpids(PROC_ALL_PIDS, 0, NULL, 0);
    pid_t *pids = calloc(n, 1);
    n = proc_listpids(PROC_ALL_PIDS, 0, pids, n) / sizeof(pid_t);
    char path[PROC_PIDPATHINFO_MAXSIZE];
    for (int i = 0; i < n; i++) {
        if (pids[i] && proc_pidpath(pids[i], path, sizeof path) > 0 &&
            strstr(path, "com.apple.Virtualization.VirtualMachine"))
            [out addObject:@(pids[i])];
    }
    free(pids);
    return out;
}

// ---------------------------------------------------------------- custom echo console

@interface EchoDev : NSObject <VZCustomVirtioDeviceConfigurationDelegate, VZCustomVirtioDeviceDelegate>
@property(strong) VZCustomVirtioDevice *dev;
@property(strong) dispatch_queue_t q;
@property(strong) NSMutableArray<VZVirtioQueueElement *> *rx;
@property(strong) NSMutableData *pending;
@property uint64_t notifies, echoed;
@property BOOL saveCalled, restoreCalled, restoreMatch;
@property(strong) NSData *restoredFrom;
@end

@implementation EchoDev
- (instancetype)init {
    if ((self = [super init])) {
        _q = dispatch_queue_create("vzprobe.echo", DISPATCH_QUEUE_SERIAL);
        _rx = [NSMutableArray array];
        _pending = [NSMutableData data];
    }
    return self;
}
- (void)customVirtioConfiguration:(VZCustomVirtioDeviceConfiguration *)c didCreateDevice:(VZCustomVirtioDevice *)d {
    self.dev = d;
    d.delegate = self;
    say(@"custom device created; shm regions=%lu", (unsigned long)d.sharedMemoryRegions.count);
}
- (void)drainRx:(VZVirtioQueue *)q {
    VZVirtioQueueElement *e;
    while ((e = [q nextElement])) [self.rx addObject:e];
}
- (void)flush {
    while (self.pending.length && self.rx.count) {
        VZVirtioQueueElement *e = self.rx.firstObject;
        [self.rx removeObjectAtIndex:0];
        NSUInteger n = MIN(self.pending.length, e.writeBuffersAvailableByteCount);
        NSError *err = nil;
        if (![e writeBuffer:(void *)self.pending.bytes exactLength:n error:&err]) say(@"rx write: %@", err);
        [e returnToQueue];
        if (self.restoreCalled) say(@"post-restore: wrote %lu bytes into an rx buffer and returned it", (unsigned long)n);
        [self.pending replaceBytesInRange:NSMakeRange(0, n) withBytes:NULL length:0];
        self.echoed += n;
    }
}
- (void)customVirtioDevice:(VZCustomVirtioDevice *)d didReceiveNotificationForQueue:(VZVirtioQueue *)q {
    self.notifies++;
    if (self.restoreCalled) say(@"post-restore notify q%u (held rx=%lu)", q.queueIndex, (unsigned long)self.rx.count);
    if (q.queueIndex == 0) {
        [self drainRx:q];
    } else if (q.queueIndex == 1) {
        VZVirtioQueueElement *e;
        while ((e = [q nextElement])) {
            for (NSData *b in [e readBuffers]) [self.pending appendData:b];
            [e returnToQueue];
        }
    }
    [self flush];
}
- (void)customVirtioDeviceDidAcceptDriverOk:(VZCustomVirtioDevice *)d {
    VZNegotiatedVirtioFeatureSet *f = d.negotiatedFeatures;
    say(@"custom device DRIVER_OK: features %08x:%08x", f.subset1, f.subset0);
    VZVirtioQueue *q0 = [d queueAtIndex:0];
    if (q0) [self drainRx:q0];
}
- (void)customVirtioDeviceWillReset:(VZCustomVirtioDevice *)d {
    say(@"custom device reset");
    [self.rx removeAllObjects];
    self.pending.length = 0;
}
- (void)customVirtioDeviceWillPause:(VZCustomVirtioDevice *)d {
    say(@"custom device pause (held rx=%lu)", (unsigned long)self.rx.count);
    if (getenv("VZPROBE_RETURN_ON_PAUSE")) {
        // A descriptor still held when the state is saved is lost on restore (measured): the
        // restored device never sees it again. Return them all, written=0; the guest reposts.
        for (VZVirtioQueueElement *e in self.rx) [e returnToQueue];
        say(@"  returned %lu held rx buffers empty", (unsigned long)self.rx.count);
        [self.rx removeAllObjects];
    }
}
- (void)customVirtioDeviceWillResume:(VZCustomVirtioDevice *)d {
    say(@"custom device resume (held rx=%lu)", (unsigned long)self.rx.count);
    VZVirtioQueue *q0 = [d queueAtIndex:0];
    if (q0) [self drainRx:q0];
    say(@"  after drain on resume: held rx=%lu", (unsigned long)self.rx.count);
}
- (void)customVirtioDeviceWillStop:(VZCustomVirtioDevice *)d { say(@"custom device stop"); }
- (NSData *)customVirtioDeviceSaveStateForRestore:(VZCustomVirtioDevice *)d {
    self.saveCalled = YES;
    NSString *s = [NSString stringWithFormat:@"echoed=%llu held_rx=%lu", self.echoed, (unsigned long)self.rx.count];
    say(@"custom device save: %@", s);
    return [s dataUsingEncoding:NSUTF8StringEncoding];
}
- (BOOL)customVirtioDeviceShouldRestore:(VZCustomVirtioDevice *)d saveState:(NSData *)saveState {
    self.restoreCalled = YES;
    self.restoredFrom = saveState;
    say(@"custom device restore: %@", [[NSString alloc] initWithData:saveState encoding:NSUTF8StringEncoding]);
    return YES;
}
@end

// ---------------------------------------------------------------- the probe

@interface Probe : NSObject <VZVirtioSocketListenerDelegate, VZVirtualMachineDelegate>
@property(strong) dispatch_queue_t vmq;
@property(strong) VZVirtualMachine *vm;
@property(strong) EchoDev *echo;
@property(strong) VZVirtioSocketListener *listener;
@property(strong) VZVirtioSocketConnection *conn, *echoConn;
@property(strong) NSURL *kernel, *initrd;
@property(copy) NSString *cmdline;
@property(strong) VZGenericMachineIdentifier *machineId;
@property(strong) NSFileHandle *logR, *logW, *echoToGuestR, *echoToGuestW, *echoFromGuestR, *echoFromGuestW;
@property pid_t vmPid;
@property(strong) id<MTLDevice> mtl;
@property(strong) id<MTLCommandQueue> cq;
@property(strong) id<MTLBuffer> buf;
@property(strong) id<MTLHeap> heap;
@property IOSurfaceRef surf;
@property void *shmPtr;
@property void *anonPtr;
@property uint64_t shmLen;
@property(copy) NSString *saveStatus;
@end

@implementation Probe

- (VZVirtualMachineConfiguration *)makeConfig {
    VZVirtualMachineConfiguration *c = [[VZVirtualMachineConfiguration alloc] init];
    VZGenericPlatformConfiguration *plat = [[VZGenericPlatformConfiguration alloc] init];
    plat.machineIdentifier = self.machineId;
    c.platform = plat;
    VZLinuxBootLoader *bl = [[VZLinuxBootLoader alloc] initWithKernelURL:self.kernel];
    bl.initialRamdiskURL = self.initrd;
    bl.commandLine = self.cmdline;
    c.bootLoader = bl;
    c.CPUCount = 2;
    c.memorySize = 2ull << 30;

    VZVirtioConsoleDeviceSerialPortConfiguration *log = [[VZVirtioConsoleDeviceSerialPortConfiguration alloc] init];
    log.attachment = [[VZFileHandleSerialPortAttachment alloc] initWithFileHandleForReading:nil fileHandleForWriting:self.logW];
    VZVirtioConsoleDeviceSerialPortConfiguration *ae = [[VZVirtioConsoleDeviceSerialPortConfiguration alloc] init];
    ae.attachment = [[VZFileHandleSerialPortAttachment alloc] initWithFileHandleForReading:self.echoToGuestR
                                                                      fileHandleForWriting:self.echoFromGuestW];
    c.serialPorts = @[ log, ae ];
    c.socketDevices = @[ [[VZVirtioSocketDeviceConfiguration alloc] init] ];
    c.memoryBalloonDevices = @[ [[VZVirtioTraditionalMemoryBalloonDeviceConfiguration alloc] init] ];
    if (@available(macOS 27, *)) {
        if (!getenv("VZPROBE_NO_CUSTOM")) [self addCustom:c];
    }
    NSError *err = nil;
    if (![c validateWithError:&err]) {
        say(@"config invalid: %@", err);
        exit(2);
    }
    if (![c validateSaveRestoreSupportWithError:&err]) say(@"save/restore NOT supported by this config: %@", err);
    else say(@"save/restore supported by this config");
    return c;
}

- (void)addCustom:(VZVirtualMachineConfiguration *)c API_AVAILABLE(macos(27.0)) {

    self.echo = [[EchoDev alloc] init];
    VZCustomVirtioDeviceConfiguration *cd = [[VZCustomVirtioDeviceConfiguration alloc] init];
    cd.deviceID = 3;  // virtio-console
    cd.PCIClassID = 0x07;
    cd.PCISubclassID = 0x80;
    cd.virtioQueueCount = 2;
    uint8_t cfg[12] = {0};
    cd.deviceSpecificConfiguration = [[VZVirtioDeviceSpecificConfiguration alloc] initWithConfigurationData:[NSData dataWithBytes:cfg length:sizeof cfg]];
    cd.sharedMemoryRegions = @[ [[VZVirtioSharedMemoryRegionConfiguration alloc] initWithRegionID:1 size:64ull << 20] ];
    cd.provider = [[VZCustomVirtioDeviceDelegateProvider alloc] initWithDeviceQueue:self.echo.q delegate:self.echo];
    cd.supportsSaveRestore = YES;
    c.customVirtioDevices = @[ cd ];
}

- (void)attachListener {
    VZVirtioSocketDevice *s = (VZVirtioSocketDevice *)self.vm.socketDevices.firstObject;
    self.listener = [[VZVirtioSocketListener alloc] init];
    self.listener.delegate = self;
    [s setSocketListener:self.listener forPort:5000];
    [s setSocketListener:self.listener forPort:5001];
}

- (void)boot {
    NSSet *before = vm_service_pids();
    self.vm = [[VZVirtualMachine alloc] initWithConfiguration:[self makeConfig] queue:self.vmq];
    self.vm.delegate = self;
    [self attachListener];
    uint64_t t0 = now_ns();
    [self.vm startWithCompletionHandler:^(NSError *e) {
        if (e) { say(@"start failed: %@", e); exit(3); }
        NSMutableSet *after = [vm_service_pids() mutableCopy];
        [after minusSet:before];
        self.vmPid = after.count == 1 ? [after.anyObject intValue] : -1;
        say(@"VM started in %.1f ms; VM service pid=%d (%lu new) self pid=%d", (now_ns() - t0) / 1e6,
            self.vmPid, (unsigned long)after.count, getpid());
    }];
}

- (BOOL)listener:(VZVirtioSocketListener *)l shouldAcceptNewConnection:(VZVirtioSocketConnection *)c fromSocketDevice:(VZVirtioSocketDevice *)d {
    int fd = dup(c.fileDescriptor);
    if (c.destinationPort == 5001) {
        self.echoConn = c;
        [NSThread detachNewThreadWithBlock:^{
            char b[4096];
            ssize_t n;
            while ((n = read(fd, b, sizeof b)) > 0) write(fd, b, n);
            close(fd);
        }];
        return YES;
    }
    self.conn = c;
    [NSThread detachNewThreadWithBlock:^{ [self serve:fd]; }];
    return YES;
}

- (void)guestDidStopVirtualMachine:(VZVirtualMachine *)vm {
    if (vm != self.vm) return;
    say(@"guest stopped the VM; notifies=%llu echoed=%llu", self.echo.notifies, self.echo.echoed);
    exit(0);
}
- (void)virtualMachine:(VZVirtualMachine *)vm didStopWithError:(NSError *)e {
    if (vm != self.vm) return;
    say(@"VM stopped with error: %@", e);
    exit(4);
}

// ---------------- command channel

static NSString *readLine(int fd, NSMutableData *buf) {
    for (;;) {
        const char *b = buf.bytes;
        for (NSUInteger i = 0; i < buf.length; i++)
            if (b[i] == '\n') {
                NSString *s = [[NSString alloc] initWithBytes:b length:i encoding:NSUTF8StringEncoding];
                [buf replaceBytesInRange:NSMakeRange(0, i + 1) withBytes:NULL length:0];
                return s;
            }
        char tmp[65536];
        ssize_t n = read(fd, tmp, sizeof tmp);
        if (n <= 0) return nil;
        [buf appendBytes:tmp length:n];
    }
}

static void reply(int fd, NSString *s) {
    say(@"  reply: %@", s);
    NSData *d = [[s stringByAppendingString:@"\n"] dataUsingEncoding:NSUTF8StringEncoding];
    write(fd, d.bytes, d.length);
}

- (void)serve:(int)fd {
    NSMutableData *buf = [NSMutableData data];
    NSString *line;
    while ((line = readLine(fd, buf))) {
        say(@"guest: %@", line);
        NSArray<NSString *> *w = [line componentsSeparatedByString:@" "];
        NSString *cmd = w.firstObject;
        if ([cmd isEqual:@"SHM_MAP"]) reply(fd, [self shmMap:w[1] len:w[2].longLongValue]);
        else if ([cmd isEqual:@"SHM_CHECK"]) reply(fd, [self shmCheck:w[1] seed:(uint32_t)w[2].longLongValue]);
        else if ([cmd isEqual:@"SHM_GPUFILL"]) reply(fd, [self shmGpuFill:w[1] val:(uint8_t)w[2].intValue]);
        else if ([cmd isEqual:@"SHM_UNMAP"]) reply(fd, [self shmUnmap]);
        else if ([cmd isEqual:@"FOOTPRINT"]) reply(fd, [self footprint]);
        else if ([cmd isEqual:@"BALLOON"]) reply(fd, [self balloon:w[1].longLongValue]);
        else if ([cmd isEqual:@"RECLAIM_BEGIN"]) {
            NSInteger n = w[2].integerValue;
            NSMutableArray *runs = [NSMutableArray arrayWithCapacity:n];
            for (NSInteger i = 0; i < n; i++) {
                NSString *r = readLine(fd, buf);
                if (!r) break;
                [runs addObject:r];
            }
            reply(fd, [self reclaim:runs mode:w[1]]);
        } else if ([cmd isEqual:@"SAVE"]) {
            reply(fd, @"OK");
            dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 500 * NSEC_PER_MSEC), self.vmq, ^{ [self saveRestore]; });
        } else if ([cmd isEqual:@"SAVESTATUS"]) reply(fd, self.saveStatus ?: @"none");
        else if ([cmd isEqual:@"DONE"]) reply(fd, @"BYE");
    }
    close(fd);
}

// ---------------- shared memory region

- (VZVirtioSharedMemoryRegion *)region API_AVAILABLE(macos(27.0)) { return self.echo.dev.sharedMemoryRegions.firstObject; }

- (NSString *)mapPtr:(void *)p len:(uint64_t)len API_AVAILABLE(macos(27.0)) {
    __block NSError *merr = nil;
    dispatch_semaphore_t s = dispatch_semaphore_create(0);
    uint64_t t0 = now_ns();
    // Every VZCustomVirtioDevice / region call asserts it is on the device queue.
    dispatch_async(self.echo.q, ^{
        [self.region mapMemory:p atOffset:0 size:len completionHandler:^(NSError *e) { merr = e; dispatch_semaphore_signal(s); }];
    });
    dispatch_semaphore_wait(s, DISPATCH_TIME_FOREVER);
    double ms = (now_ns() - t0) / 1e6;
    if (merr) return [NSString stringWithFormat:@"FAIL map %.2fms %@", ms, merr.localizedDescription];
    return [NSString stringWithFormat:@"map_ms=%.2f", ms];
}

- (NSString *)shmMap:(NSString *)kind len:(uint64_t)len API_AVAILABLE(macos(27.0)) {
    if (!self.mtl) { self.mtl = MTLCreateSystemDefaultDevice(); self.cq = [self.mtl newCommandQueue]; }
    self.shmLen = len;
    void *p = NULL;
    if ([kind isEqual:@"anon"] || [kind isEqual:@"mtlnocopy"]) {
        p = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_ANON | MAP_PRIVATE, -1, 0);
        self.anonPtr = p;
        if ([kind isEqual:@"mtlnocopy"])
            self.buf = [self.mtl newBufferWithBytesNoCopy:p length:len options:MTLResourceStorageModeShared deallocator:nil];
    } else if ([kind isEqual:@"mtl"]) {
        self.buf = [self.mtl newBufferWithLength:len options:MTLResourceStorageModeShared];
        p = self.buf.contents;
    } else if ([kind isEqual:@"heap"]) {
        MTLHeapDescriptor *hd = [[MTLHeapDescriptor alloc] init];
        hd.storageMode = MTLStorageModeShared;
        hd.type = MTLHeapTypePlacement;
        hd.size = len;
        self.heap = [self.mtl newHeapWithDescriptor:hd];
        self.buf = [self.heap newBufferWithLength:len options:MTLResourceStorageModeShared offset:0];
        p = self.buf.contents;
    } else if ([kind isEqual:@"iosurface"]) {
        int width = 4096, height = (int)(len / (4096 * 4));
        self.surf = IOSurfaceCreate((__bridge CFDictionaryRef) @{
            (id)kIOSurfaceWidth : @(width), (id)kIOSurfaceHeight : @(height),
            (id)kIOSurfaceBytesPerElement : @4, (id)kIOSurfacePixelFormat : @((uint32_t)'BGRA')});
        IOSurfaceLock(self.surf, 0, NULL);
        p = IOSurfaceGetBaseAddress(self.surf);
        say(@"iosurface alloc size=%zu bpr=%zu", IOSurfaceGetAllocSize(self.surf), IOSurfaceGetBytesPerRow(self.surf));
    }
    if (!p || p == MAP_FAILED) return @"FAIL alloc";
    self.shmPtr = p;
    uint32_t seed = arc4random();
    uint32_t *wds = p;
    for (uint64_t i = 0; i < len / 8; i++) wds[i] = (uint32_t)i ^ seed;
    NSString *m = [self mapPtr:p len:len];
    say(@"shm %@: ptr=%p page-aligned=%d %@", kind, p, ((uintptr_t)p & 16383) == 0, m);
    if ([m hasPrefix:@"FAIL"]) { [self release_]; return m; }
    return [NSString stringWithFormat:@"OK %u %@", seed, m];
}

- (NSString *)shmCheck:(NSString *)kind seed:(uint32_t)seed2 {
    uint64_t words = self.shmLen / 4, half = words / 2, cpuBad = 0;
    uint32_t *w = self.shmPtr;
    for (uint64_t i = half; i < words; i++)
        if (w[i] != (~(uint32_t)i ^ seed2)) cpuBad++;
    NSString *gpu = @"gpu_read=n/a";
    if (self.buf) {
        id<MTLBuffer> dst = [self.mtl newBufferWithLength:half * 4 options:MTLResourceStorageModeShared];
        id<MTLCommandBuffer> cb = [self.cq commandBuffer];
        id<MTLBlitCommandEncoder> be = [cb blitCommandEncoder];
        [be copyFromBuffer:self.buf sourceOffset:half * 4 toBuffer:dst destinationOffset:0 size:half * 4];
        [be endEncoding];
        [cb commit];
        [cb waitUntilCompleted];
        uint32_t *g = dst.contents;
        uint64_t gb = 0;
        for (uint64_t i = 0; i < half; i++)
            if (g[i] != (~(uint32_t)(i + half) ^ seed2)) gb++;
        gpu = [NSString stringWithFormat:@"gpu_read_bad=%llu", gb];
    }
    return [NSString stringWithFormat:@"cpu_read_bad=%llu/%llu %@", cpuBad, half, gpu];
}

- (NSString *)shmGpuFill:(NSString *)kind val:(uint8_t)v {
    if (!self.buf) return @"FAIL no buffer";
    id<MTLCommandBuffer> cb = [self.cq commandBuffer];
    id<MTLBlitCommandEncoder> be = [cb blitCommandEncoder];
    [be fillBuffer:self.buf range:NSMakeRange(0, self.shmLen) value:v];
    [be endEncoding];
    [cb commit];
    [cb waitUntilCompleted];
    return cb.error ? [NSString stringWithFormat:@"FAIL %@", cb.error] : @"OK";
}

- (void)release_ {
    if (self.surf) { IOSurfaceUnlock(self.surf, 0, NULL); CFRelease(self.surf); self.surf = NULL; }
    self.buf = nil;
    self.heap = nil;
    if (self.anonPtr) { munmap(self.anonPtr, self.shmLen); self.anonPtr = NULL; }
    self.shmPtr = NULL;
}

- (NSString *)shmUnmap API_AVAILABLE(macos(27.0)) {
    __block NSError *uerr = nil;
    dispatch_semaphore_t s = dispatch_semaphore_create(0);
    uint64_t t0 = now_ns();
    uint64_t len = self.shmLen;
    dispatch_async(self.echo.q, ^{
        [self.region unmapMemoryAtOffset:0 size:len completionHandler:^(NSError *e) { uerr = e; dispatch_semaphore_signal(s); }];
    });
    dispatch_semaphore_wait(s, DISPATCH_TIME_FOREVER);
    double ms = (now_ns() - t0) / 1e6;
    [self release_];
    return uerr ? [NSString stringWithFormat:@"FAIL unmap %@", uerr.localizedDescription] : [NSString stringWithFormat:@"OK unmap_ms=%.2f", ms];
}

// ---------------- Apple's balloon

- (NSString *)balloon:(uint64_t)mib {
    __block NSString *r = @"FAIL no balloon";
    dispatch_sync(self.vmq, ^{
        VZVirtioTraditionalMemoryBalloonDevice *b = (VZVirtioTraditionalMemoryBalloonDevice *)self.vm.memoryBalloonDevices.firstObject;
        if (!b) return;
        b.targetVirtualMachineMemorySize = (2ull << 30) - (mib << 20);
        r = [NSString stringWithFormat:@"OK target_MiB=%llu", b.targetVirtualMachineMemorySize >> 20];
    });
    return r;
}

// ---------------- reclaim

- (NSString *)footprint {
    return [NSString stringWithFormat:@"vm_service_fp_MiB=%.0f self_fp_MiB=%.0f", footprint_mib(self.vmPid), footprint_mib(getpid())];
}

- (NSString *)reclaim:(NSArray<NSString *> *)runs mode:(NSString *)mode API_AVAILABLE(macos(27.0)) {
    NSString *before = [self footprint];
    uint64_t total = 0, failed = 0, nomap = 0;
    int lastErr = 0;
    NSMutableArray *keep = [NSMutableArray array];
    uint64_t t0 = now_ns();
    for (NSString *r in runs) {
        NSArray *p = [r componentsSeparatedByString:@" "];
        uint64_t gpa = strtoull([p[0] UTF8String], NULL, 10), len = strtoull([p[1] UTF8String], NULL, 10);
        __block VZGuestMemoryMapping *m = nil;
        dispatch_sync(self.echo.q, ^{ m = [self.echo.dev guestMemoryMappingAtPhysicalAddress:gpa length:len]; });
        if (!m) { nomap++; continue; }
        [keep addObject:m];
        int adv = [mode isEqual:@"dontneed"] ? MADV_DONTNEED : [mode isEqual:@"free"] ? MADV_FREE : MADV_FREE_REUSABLE;
        if (madvise(m.mutableBytes, len, adv) != 0) { failed++; lastErr = errno; }
        else total += len;
    }
    double ms = (now_ns() - t0) / 1e6;
    sleep(1);
    NSString *after = [self footprint];
    return [NSString stringWithFormat:@"mode=%@ runs=%lu advised_MiB=%llu madvise_failed=%llu(errno %d) no_mapping=%llu took_ms=%.1f before{%@} after{%@}",
                                      mode, (unsigned long)runs.count, total >> 20, failed, lastErr, nomap, ms, before, after];
}

// ---------------- save / restore

- (void)saveRestore {
    NSURL *url = [NSURL fileURLWithPath:[NSString stringWithFormat:@"/tmp/vzprobe-%d.vzvmsave", getpid()]];
    [[NSFileManager defaultManager] removeItemAtURL:url error:nil];
    uint64_t t0 = now_ns();
    EchoDev *oldEcho = self.echo;
    [self.vm pauseWithCompletionHandler:^(NSError *pe) {
        if (pe) { self.saveStatus = [NSString stringWithFormat:@"pause failed %@", pe]; [self.vm resumeWithCompletionHandler:^(NSError *x){}]; return; }
        [self.vm saveMachineStateToURL:url completionHandler:^(NSError *se) {
            double tSave = (now_ns() - t0) / 1e6;
            if (se) {
                self.saveStatus = [NSString stringWithFormat:@"save failed after %.0fms: %@", tSave, se];
                say(@"%@", self.saveStatus);
                [self.vm resumeWithCompletionHandler:^(NSError *x){}];
                return;
            }
            NSNumber *sz = [[NSFileManager defaultManager] attributesOfItemAtPath:url.path error:nil][NSFileSize];
            say(@"saved in %.0f ms (%@ bytes); stopping and restoring into a fresh VM", tSave, sz);
            [self.vm stopWithCompletionHandler:^(NSError *ste) {
                VZVirtualMachine *nvm = [[VZVirtualMachine alloc] initWithConfiguration:[self makeConfig] queue:self.vmq];
                self.vm = nvm;
                nvm.delegate = self;
                [self attachListener];
                uint64_t t1 = now_ns();
                NSSet *before = vm_service_pids();
                [nvm restoreMachineStateFromURL:url completionHandler:^(NSError *re) {
                    if (re) { self.saveStatus = [NSString stringWithFormat:@"restore failed: %@", re]; say(@"%@", self.saveStatus); exit(5); }
                    [nvm resumeWithCompletionHandler:^(NSError *ue) {
                        NSMutableSet *after = [vm_service_pids() mutableCopy];
                        [after minusSet:before];
                        if (after.count == 1) self.vmPid = [after.anyObject intValue];
                        double tRest = (now_ns() - t1) / 1e6;
                        NSString *want = [[NSString alloc] initWithData:[oldEcho customVirtioDeviceSaveStateForRestore:oldEcho.dev] encoding:NSUTF8StringEncoding];
                        (void)want;
                        self.saveStatus = [NSString stringWithFormat:@"save_ms=%.0f file_bytes=%@ restore_resume_ms=%.0f resume_err=%@ old_save_called=%d new_restore_called=%d restored_data='%@'",
                                                                     tSave, sz, tRest, ue, oldEcho.saveCalled, self.echo.restoreCalled,
                                                                     [[NSString alloc] initWithData:self.echo.restoredFrom encoding:NSUTF8StringEncoding]];
                        say(@"%@", self.saveStatus);
                    }];
                }];
            }];
        }];
    }];
}

@end

int main(int argc, char **argv) {
    @autoreleasepool {
        if (argc < 3) { fprintf(stderr, "usage: %s <kernel> <initrd> [extra cmdline]\n", argv[0]); return 1; }
        say(@"maximumAllowedSharedMemoryRegionCount=%lu", (unsigned long)VZCustomVirtioDeviceConfiguration.maximumAllowedSharedMemoryRegionCount);
        Probe *p = [[Probe alloc] init];
        p.vmq = dispatch_queue_create("vzprobe.vm", DISPATCH_QUEUE_SERIAL);
        p.kernel = [NSURL fileURLWithPath:@(argv[1])];
        p.initrd = [NSURL fileURLWithPath:@(argv[2])];
        p.cmdline = [NSString stringWithFormat:@"console=hvc0 rdinit=/init %s", argc > 3 ? argv[3] : ""];
        p.machineId = [[VZGenericMachineIdentifier alloc] init];
        NSPipe *log = [NSPipe pipe], *toGuest = [NSPipe pipe], *fromGuest = [NSPipe pipe];
        p.logW = log.fileHandleForWriting;
        p.echoToGuestR = toGuest.fileHandleForReading;
        p.echoToGuestW = toGuest.fileHandleForWriting;
        p.echoFromGuestR = fromGuest.fileHandleForReading;
        p.echoFromGuestW = fromGuest.fileHandleForWriting;
        int logfd = log.fileHandleForReading.fileDescriptor;
        [NSThread detachNewThreadWithBlock:^{
            char b[4096];
            ssize_t n;
            while ((n = read(logfd, b, sizeof b)) > 0) { fwrite(b, 1, n, stdout); fflush(stdout); }
        }];
        int ein = fromGuest.fileHandleForReading.fileDescriptor, eout = toGuest.fileHandleForWriting.fileDescriptor;
        [NSThread detachNewThreadWithBlock:^{
            uint8_t b[65536];
            ssize_t n;
            while ((n = read(ein, b, sizeof b)) > 0) {
                for (ssize_t i = 0; i < n; i++) b[i] ^= 0x20;
                write(eout, b, n);
            }
        }];
        dispatch_async(p.vmq, ^{ [p boot]; });
        dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 900 * NSEC_PER_SEC), dispatch_get_main_queue(), ^{ say(@"watchdog: 900 s, giving up"); exit(9); });
        dispatch_main();
    }
}
