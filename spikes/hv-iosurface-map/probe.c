// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva
//
// Spike 3 of docs/design/blob-decode-targets.md: can an IOSurface's memory be
// mapped into a guest at all?
//
// This is the branching spike. The design wants one IOSurface to be the decode
// target's storage — mapped into the guest as a BO, bound by the GPU, and handed
// to scanout by id — but nothing in the stack does that today. venus maps
// shm-backed vkMapMemory pointers, and zero-copy scanout passes surfaces by id
// and never maps them. IOKit-owned pages are not obviously anonymous pages: they
// may refuse hv_vm_map outright, or map and then not stay coherent, or not stay
// put across the IOSurfaceLock protocol the guest can never take part in.
//
// If this is green, phase 1 is a guest BO backed directly by the target surface.
// If it is red, the target's storage becomes an ordinary host-visible blob, the
// host copies the decoded frame there instead, and phase 2's zero-copy present
// has to be redesigned. The per-frame copy cost is the same either way
// (spikes/vt-blob-decode-target).
//
// Build, sign and run with ./run.sh — hv_vm_create needs
// com.apple.security.hypervisor.
//
// The vehicle is the one from spikes/balloon-unmap-fault: a bare HVF VM, one
// vCPU at EL1h with the MMU off (so guest virtual == IPA), MMIO by data-abort.
// Guest code is emitted inline rather than assembled, so there is no build step
// beyond clang.

#include <Hypervisor/Hypervisor.h>
#include <IOSurface/IOSurface.h>

#include <mach/mach_time.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

#define PAGE_16K 16384ULL

#define RAM_BASE 0x80000000ULL /* guest code + stack, ordinary anonymous memory */
#define RAM_SIZE (1ULL << 20)

#define SURF_IPA 0x400000000ULL /* where the IOSurface gets mapped */

#define MMIO_BASE 0x10000000ULL
#define M_VALUE 0x00 /* guest reports one 64-bit observation */
#define M_DONE 0x08
#define M_SYNC 0x10 /* guest parks here while the host cycles the surface lock */

#define BOOT_CPSR 0x3C5ULL /* EL1h, DAIF masked */
#define WATCHDOG_NS (10ULL * 1000 * 1000 * 1000)

/* What the guest writes where the host will look for it. */
#define GUEST_MARK 0x6C696D696E610001ULL

static const char *hv_err_name(hv_return_t r)
{
    switch ((uint32_t)r) {
    case 0: return "HV_SUCCESS";
    case 0xfae94001: return "HV_ERROR";
    case 0xfae94002: return "HV_BUSY";
    case 0xfae94003: return "HV_BAD_ARGUMENT";
    case 0xfae94005: return "HV_NO_RESOURCES";
    case 0xfae94006: return "HV_NO_DEVICE";
    case 0xfae94007: return "HV_DENIED";
    case 0xfae94008: return "HV_FAULT";
    case 0xfae9400f: return "HV_UNSUPPORTED";
    default: return "HV_???";
    }
}

#define CHECK(expr)                                                                     \
    do {                                                                                \
        hv_return_t _r = (expr);                                                        \
        if (_r != HV_SUCCESS) {                                                         \
            fprintf(stderr, "FATAL %s:%d %s -> 0x%x (%s)\n", __FILE__, __LINE__, #expr, \
                    (uint32_t)_r, hv_err_name(_r));                                     \
            exit(1);                                                                    \
        }                                                                               \
    } while (0)

static uint64_t now_ns(void)
{
    static mach_timebase_info_data_t tb;
    if (tb.denom == 0)
        mach_timebase_info(&tb);
    return mach_absolute_time() * tb.numer / tb.denom;
}

static _Atomic uint64_t g_deadline_ns;
static _Atomic bool g_watch_on;
static hv_vcpu_t g_vcpu;

static void *watchdog_main(void *arg)
{
    (void)arg;
    for (;;) {
        usleep(100 * 1000);
        if (atomic_load(&g_watch_on) && now_ns() > atomic_load(&g_deadline_ns)) {
            hv_vcpu_t v = g_vcpu;
            hv_vcpus_exit(&v, 1);
        }
    }
    return NULL;
}

/* ------------------------------------------------------- guest code emitter */

struct emit {
    uint32_t *out;
    size_t n;
};

static void ins(struct emit *e, uint32_t word)
{
    e->out[e->n++] = word;
}

/* movz/movk sequence for an arbitrary 64-bit constant into Xd. */
static void emit_imm64(struct emit *e, int rd, uint64_t v)
{
    ins(e, 0xD2800000u | (uint32_t)(((v >> 0) & 0xffff) << 5) | (uint32_t)rd); /* movz */
    for (int hw = 1; hw < 4; hw++) {
        uint32_t chunk = (uint32_t)((v >> (16 * hw)) & 0xffff);
        if (chunk)
            ins(e, 0xF2800000u | ((uint32_t)hw << 21) | (chunk << 5) | (uint32_t)rd); /* movk */
    }
}

static void emit_ldr(struct emit *e, int rt, int rn)
{
    ins(e, 0xF9400000u | ((uint32_t)rn << 5) | (uint32_t)rt);
}

static void emit_str(struct emit *e, int rt, int rn)
{
    ins(e, 0xF9000000u | ((uint32_t)rn << 5) | (uint32_t)rt);
}

/* Read 8 bytes at `addr` and report them to the host over MMIO. */
static void emit_probe_read(struct emit *e, uint64_t addr)
{
    emit_imm64(e, 1, addr);
    emit_ldr(e, 2, 1);
    emit_imm64(e, 0, MMIO_BASE + M_VALUE);
    emit_str(e, 2, 0);
}

/* Store `val` at `addr` — the host looks for it through the IOSurface afterwards. */
static void emit_probe_write(struct emit *e, uint64_t addr, uint64_t val)
{
    emit_imm64(e, 1, addr);
    emit_imm64(e, 2, val);
    emit_str(e, 2, 1);
}

/* Park until the host has done something underneath us. The read is answered by
 * the run loop, which performs the IOSurfaceLock cycle during the exit. */
static void emit_sync(struct emit *e)
{
    emit_imm64(e, 0, MMIO_BASE + M_SYNC);
    emit_ldr(e, 2, 0);
}

static void emit_done(struct emit *e)
{
    emit_imm64(e, 0, MMIO_BASE + M_DONE);
    emit_imm64(e, 2, 1);
    emit_str(e, 2, 0);
    ins(e, 0x14000000u); /* b . — parked; the host stops at DONE */
}

/* ---------------------------------------------------------------- run state */

#define MAX_OBS 32

struct run_state {
    uint64_t obs[MAX_OBS];
    int nobs;
    bool done;
    uint64_t fault_pa;
    bool faulted;
};

/* Set up before the run; the M_SYNC handler needs them mid-exit. */
static IOSurfaceRef g_surf;
static uint8_t *g_base;
static size_t g_mark_off;   /* where the guest wrote its marker */
static size_t g_repeat_off; /* where the host plants a fresh value after the cycle */

/* Filled in by the M_SYNC handler. */
static uint64_t g_seen_locked;
static uint8_t *g_base_after;
static uint64_t g_seen_relock;
static const uint64_t REPEAT_VAL = 0x5A5A5A5A5A5A5A5AULL;

/* Q3 and Q4, performed while the guest is parked on its MMIO read. The guest can
 * never take part in the IOSurfaceLock protocol, so the mapping has to survive
 * the host cycling the lock underneath it. */
static void do_sync(void)
{
    g_seen_locked = *(volatile uint64_t *)(g_base + g_mark_off);
    IOSurfaceUnlock(g_surf, 0, NULL);
    IOSurfaceLock(g_surf, 0, NULL);
    g_base_after = IOSurfaceGetBaseAddress(g_surf);
    g_seen_relock = *(volatile uint64_t *)(g_base_after + g_mark_off);
    *(volatile uint64_t *)(g_base_after + g_repeat_off) = REPEAT_VAL;
}

static bool run_guest(hv_vcpu_t vcpu, hv_vcpu_exit_t *vexit, struct run_state *st)
{
    atomic_store(&g_deadline_ns, now_ns() + WATCHDOG_NS);
    atomic_store(&g_watch_on, true);

    for (;;) {
        CHECK(hv_vcpu_run(vcpu));

        if (vexit->reason == HV_EXIT_REASON_CANCELED) {
            uint64_t pc = 0;
            hv_vcpu_get_reg(vcpu, HV_REG_PC, &pc);
            fprintf(stderr, "  WATCHDOG: no progress; pc=0x%llx\n", pc);
            atomic_store(&g_watch_on, false);
            return false;
        }
        if (vexit->reason != HV_EXIT_REASON_EXCEPTION) {
            fprintf(stderr, "  unexpected exit reason %u\n", vexit->reason);
            atomic_store(&g_watch_on, false);
            return false;
        }

        uint64_t syn = vexit->exception.syndrome;
        uint64_t ec = (syn >> 26) & 0x3f;
        uint64_t pa = vexit->exception.physical_address;

        if (ec != 0x24) { /* not a data abort from a lower EL */
            uint64_t pc = 0;
            hv_vcpu_get_reg(vcpu, HV_REG_PC, &pc);
            fprintf(stderr, "  unhandled exception ec=0x%llx pc=0x%llx pa=0x%llx\n", ec, pc, pa);
            atomic_store(&g_watch_on, false);
            return false;
        }

        /* A fault inside the surface window is the interesting failure: the
         * mapping was accepted but does not actually back guest accesses. */
        if (pa >= SURF_IPA && pa < SURF_IPA + (1ULL << 30)) {
            fprintf(stderr, "  DATA ABORT inside the surface window: pa=0x%llx syndrome=0x%llx\n",
                    pa, syn);
            st->faulted = true;
            st->fault_pa = pa;
            atomic_store(&g_watch_on, false);
            return false;
        }

        bool iswrite = (syn >> 6) & 1;
        uint32_t srt = (syn >> 16) & 0x1f;
        uint64_t pc = 0;
        CHECK(hv_vcpu_get_reg(vcpu, HV_REG_PC, &pc));
        CHECK(hv_vcpu_set_reg(vcpu, HV_REG_PC, pc + 4));

        if (!iswrite) {
            if (pa - MMIO_BASE == M_SYNC)
                do_sync();
            if (srt < 31)
                CHECK(hv_vcpu_set_reg(vcpu, HV_REG_X0 + srt, 0));
            continue;
        }

        uint64_t val = 0;
        if (srt < 31)
            CHECK(hv_vcpu_get_reg(vcpu, HV_REG_X0 + srt, &val));

        switch (pa - MMIO_BASE) {
        case M_VALUE:
            if (st->nobs < MAX_OBS)
                st->obs[st->nobs++] = val;
            break;
        case M_DONE:
            st->done = true;
            atomic_store(&g_watch_on, false);
            return true;
        default:
            fprintf(stderr, "  unexpected MMIO write pa=0x%llx\n", pa);
            break;
        }
    }
}

/* ------------------------------------------------------------- the surface */

static void set_num(CFMutableDictionaryRef d, CFStringRef key, int64_t v)
{
    CFNumberRef n = CFNumberCreate(kCFAllocatorDefault, kCFNumberSInt64Type, &v);
    CFDictionarySetValue(d, key, n);
    CFRelease(n);
}

/* NV12 at a caller-chosen luma pitch, sized the way spikes/vt-blob-decode-target
 * established a real allocator must: each plane in its own right, because chroma
 * needs ceil(w/2)*2 bytes per row and cannot inherit luma's pitch. */
static IOSurfaceRef make_nv12(size_t w, size_t h, size_t luma_pitch)
{
    size_t chroma_min = ((w + 1) / 2) * 2;
    size_t chroma_pitch = luma_pitch < chroma_min ? chroma_min : luma_pitch;
    size_t luma_size = luma_pitch * h;
    size_t chroma_size = chroma_pitch * ((h + 1) / 2);

    CFMutableArrayRef planes = CFArrayCreateMutable(kCFAllocatorDefault, 2, &kCFTypeArrayCallBacks);
    struct {
        size_t w, h, bpr, off, size, bpe;
    } p[2] = {
        {w, h, luma_pitch, 0, luma_size, 1},
        {(w + 1) / 2, (h + 1) / 2, chroma_pitch, luma_size, chroma_size, 2},
    };
    for (int i = 0; i < 2; i++) {
        CFMutableDictionaryRef pd = CFDictionaryCreateMutable(
            kCFAllocatorDefault, 6, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
        set_num(pd, kIOSurfacePlaneWidth, (int64_t)p[i].w);
        set_num(pd, kIOSurfacePlaneHeight, (int64_t)p[i].h);
        set_num(pd, kIOSurfacePlaneBytesPerRow, (int64_t)p[i].bpr);
        set_num(pd, kIOSurfacePlaneOffset, (int64_t)p[i].off);
        set_num(pd, kIOSurfacePlaneSize, (int64_t)p[i].size);
        set_num(pd, kIOSurfacePlaneBytesPerElement, (int64_t)p[i].bpe);
        CFArrayAppendValue(planes, pd);
        CFRelease(pd);
    }

    CFMutableDictionaryRef d = CFDictionaryCreateMutable(
        kCFAllocatorDefault, 5, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    set_num(d, kIOSurfaceWidth, (int64_t)w);
    set_num(d, kIOSurfaceHeight, (int64_t)h);
    set_num(d, kIOSurfacePixelFormat, (int64_t)0x34323076 /* '420v' */);
    set_num(d, kIOSurfaceAllocSize, (int64_t)(luma_size + chroma_size));
    CFDictionarySetValue(d, kIOSurfacePlaneInfo, planes);
    CFRelease(planes);

    IOSurfaceRef s = IOSurfaceCreate(d);
    CFRelease(d);
    return s;
}

/* ---------------------------------------------------------------------- main */

int main(void)
{
    /* 4K NV12 — big enough that the mapping spans many pages, which is the case
     * a decode target actually is. */
    const size_t W = 3840, H = 2160;
    IOSurfaceRef surf = make_nv12(W, H, W);
    if (!surf) {
        fprintf(stderr, "IOSurfaceCreate failed\n");
        return 1;
    }

    IOSurfaceLock(surf, 0, NULL);
    uint8_t *base = IOSurfaceGetBaseAddress(surf);
    size_t alloc = IOSurfaceGetAllocSize(surf);
    size_t luma_bpr = IOSurfaceGetBytesPerRowOfPlane(surf, 0);
    size_t chroma_off =
        (size_t)((uint8_t *)IOSurfaceGetBaseAddressOfPlane(surf, 1) - base);

    printf("=== the surface ===\n");
    printf("  %zux%zu NV12, id %u, allocSize %zu, luma bytesPerRow %zu, chroma offset %zu\n", W, H,
           IOSurfaceGetID(surf), alloc, luma_bpr, chroma_off);
    printf("  base address %p  (%% 16384 = %llu, %% 4096 = %llu)\n", base,
           (unsigned long long)((uintptr_t)base % PAGE_16K),
           (unsigned long long)((uintptr_t)base % 4096));

    /* hv_vm_map takes granule-multiple sizes; map as much of the surface as fits. */
    size_t map_size = alloc & ~(PAGE_16K - 1);
    printf("  mapping %zu of %zu bytes at IPA 0x%llx\n\n", map_size, alloc, SURF_IPA);

    if ((uintptr_t)base % PAGE_16K != 0)
        printf("  NOTE: base is not 16 KiB-aligned; hv_vm_map is expected to refuse it.\n\n");

    /* Host-side pattern the guest must be able to read back. One 64-bit word per
     * probe point, placed to cover the start, the middle, the last mapped page,
     * and the chroma plane — a mapping that only backs its first page would pass
     * a single-offset test. */
    struct {
        const char *name;
        size_t off;
    } points[] = {
        {"luma byte 0", 0},
        {"luma mid-plane", (luma_bpr * H) / 2 & ~7ULL},
        {"chroma plane start", chroma_off},
        {"last mapped page", (map_size - PAGE_16K) & ~7ULL},
    };
    const int npoints = (int)(sizeof(points) / sizeof(points[0]));

    for (int i = 0; i < npoints; i++)
        *(volatile uint64_t *)(base + points[i].off) = 0xA5A5000000000000ULL | (uint64_t)i;

    pthread_t wt;
    pthread_create(&wt, NULL, watchdog_main, NULL);

    CHECK(hv_vm_create(NULL));

    uint8_t *ram = mmap(NULL, RAM_SIZE, PROT_READ | PROT_WRITE, MAP_ANON | MAP_PRIVATE, -1, 0);
    if (ram == MAP_FAILED) {
        perror("mmap");
        return 1;
    }
    CHECK(hv_vm_map(ram, RAM_BASE, RAM_SIZE, HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC));

    /* Q1 — will hv_vm_map take IOKit-owned pages at all? */
    printf("=== Q1: does hv_vm_map accept an IOSurface's base address? ===\n");
    hv_return_t r = hv_vm_map(base, SURF_IPA, map_size, HV_MEMORY_READ | HV_MEMORY_WRITE);
    printf("  hv_vm_map(IOSurface base) -> %s (0x%x)\n\n", hv_err_name(r), (uint32_t)r);
    if (r != HV_SUCCESS) {
        printf("VERDICT: RED — an IOSurface cannot back a guest BO directly.\n");
        printf("The decode target's storage has to be an ordinary host-visible blob, with\n");
        printf("the host copying the decoded frame into it; phase 2's zero-copy present\n");
        printf("needs redesigning around that.\n");
        IOSurfaceUnlock(surf, 0, NULL);
        return 1;
    }

    /* One payload for the whole run. An earlier version rewrote the payload
     * between runs and reset PC, and the vCPU re-executed the STALE instructions
     * still in its I-cache — reporting a value from the wrong offset, which read
     * exactly like a coherency failure. Emit once; rendezvous with the host over
     * MMIO instead. */
    g_surf = surf;
    g_base = base;
    g_mark_off = points[0].off;
    g_repeat_off = points[1].off;

    struct emit e = {.out = (uint32_t *)ram, .n = 0};
    for (int i = 0; i < npoints; i++)
        emit_probe_read(&e, SURF_IPA + points[i].off);   /* Q2 */
    emit_probe_write(&e, SURF_IPA + points[0].off, GUEST_MARK); /* Q3 */
    emit_sync(&e);                                       /* host does Q3/Q4 here */
    emit_probe_read(&e, SURF_IPA + points[1].off);       /* Q4, after the cycle */
    emit_done(&e);

    hv_vcpu_t vcpu;
    hv_vcpu_exit_t *vexit;
    CHECK(hv_vcpu_create(&vcpu, &vexit, NULL));
    g_vcpu = vcpu;
    CHECK(hv_vcpu_set_reg(vcpu, HV_REG_CPSR, BOOT_CPSR));
    CHECK(hv_vcpu_set_reg(vcpu, HV_REG_PC, RAM_BASE));

    struct run_state st = {0};
    bool ran = run_guest(vcpu, vexit, &st);

    printf("=== Q2: does the guest read what the host wrote? ===\n");
    int matched = 0;
    for (int i = 0; i < npoints; i++) {
        uint64_t want = 0xA5A5000000000000ULL | (uint64_t)i;
        bool ok = i < st.nobs && st.obs[i] == want;
        matched += ok;
        printf("  %-20s off %9zu  guest saw 0x%016llx  %s\n", points[i].name, points[i].off,
               i < st.nobs ? st.obs[i] : 0, ok ? "match" : "MISMATCH");
    }
    if (st.faulted)
        printf("  guest faulted at pa=0x%llx\n", st.fault_pa);

    printf("\n=== Q3: does the host see the guest's write? ===\n");
    printf("  through the held lock          0x%016llx  %s\n", g_seen_locked,
           g_seen_locked == GUEST_MARK ? "match" : "MISMATCH");

    printf("\n=== Q4: does the mapping survive an IOSurfaceLock cycle? ===\n");
    printf("  base address after unlock/lock %p  %s\n", g_base_after,
           g_base_after == base ? "unchanged" : "MOVED — the stale mapping aliases nothing");
    printf("  guest's write still readable   0x%016llx  %s\n", g_seen_relock,
           g_seen_relock == GUEST_MARK ? "yes" : "NO");
    uint64_t post = st.nobs > npoints ? st.obs[npoints] : 0;
    bool post_ok = post == REPEAT_VAL;
    printf("  guest read after the cycle     0x%016llx  %s\n", post,
           post_ok ? "match" : "MISMATCH");

    printf("\n=== verdict ===\n");
    bool green = ran && st.done && matched == npoints && !st.faulted &&
                 g_seen_locked == GUEST_MARK && g_base_after == base &&
                 g_seen_relock == GUEST_MARK && post_ok;
    printf("  Q1 map accepted      : yes\n");
    printf("  Q2 guest reads host  : %d of %d probe points\n", matched, npoints);
    printf("  Q3 host reads guest  : %s\n", g_seen_locked == GUEST_MARK ? "yes" : "no");
    printf("  Q4 survives lock cyc : %s\n", post_ok && g_base_after == base ? "yes" : "no");
    printf("\nVERDICT: %s\n", green ? "GREEN — an IOSurface can back a guest BO directly"
                                     : "RED — see the failing arm above");

    CHECK(hv_vcpu_destroy(vcpu));
    hv_vm_unmap(SURF_IPA, map_size);
    hv_vm_unmap(RAM_BASE, RAM_SIZE);
    CHECK(hv_vm_destroy());
    IOSurfaceUnlock(surf, 0, NULL);
    CFRelease(surf);
    return green ? 0 : 1;
}
