// Does MADV_ZERO free compressor-held copies of anonymous pages — and does
// MADV_FREE_REUSABLE leave them billed?
//
// Field observation (the dogfood Mac, 2026-08-11, post unmap-fix): with the balloon at ~18 GiB
// released, the worker still bills ~14 GB of compressor/swap-held dirty pages, far
// more than live guest RAM can account for. Hypothesis: pages compressed BEFORE the
// balloon released their range stay in the compressor (billed, holding swap) because
// MADV_FREE_REUSABLE's reuse walk only visits resident pages, and the pageout scan
// never revisits compressor slots.
//
// This probe, on the real host kernel:
//   1. dirties a 2 GiB anonymous target region,
//   2. forces it into the compressor (MADV_PAGEOUT if permitted, else memory ballast),
//   3. applies a different advice recipe to each 512 MiB quarter:
//        Q1 MADV_FREE_REUSABLE only        (the balloon's current release behavior)
//        Q2 MADV_ZERO only
//        Q3 MADV_ZERO then MADV_FREE_REUSABLE
//        Q4 MADV_FREE_REUSABLE then MADV_ZERO
//   4. snapshots task_vm_info (phys_footprint, compressed) around every step and
//      counts resident pages per quarter via mincore,
//   5. verifies post-advice reads: Q2-Q4 must read zero; times each madvise call.
//
// Build/run: ./build.sh && ./probe   (no entitlements needed — pure VM, no HVF)

#include <errno.h>
#include <mach/mach.h>
#include <mach/mach_time.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/sysctl.h>
#include <unistd.h>

#ifndef MADV_PAGEOUT
#define MADV_PAGEOUT 10
#endif
#ifndef MADV_ZERO
#define MADV_ZERO 11
#endif

#define GIB (1ULL << 30)
#define TARGET_SIZE (2 * GIB)
#define QUARTER (TARGET_SIZE / 4)
#define BALLAST_CHUNK GIB
#define BALLAST_MAX 22ULL
#define PATTERN 0x5a

static uint64_t now_ns(void) {
    static mach_timebase_info_data_t tb;
    if (tb.denom == 0)
        mach_timebase_info(&tb);
    return mach_absolute_time() * tb.numer / tb.denom;
}

struct snap {
    uint64_t footprint;
    uint64_t compressed;
    uint64_t resident;
};

static struct snap take_snap(void) {
    task_vm_info_data_t info;
    mach_msg_type_number_t count = TASK_VM_INFO_COUNT;
    kern_return_t kr =
        task_info(mach_task_self(), TASK_VM_INFO, (task_info_t)&info, &count);
    if (kr != KERN_SUCCESS) {
        fprintf(stderr, "task_info: %d\n", kr);
        exit(1);
    }
    struct snap s = {.footprint = info.phys_footprint,
                     .compressed = info.compressed,
                     .resident = info.resident_size};
    return s;
}

static void print_snap(const char *label, struct snap s) {
    printf("%-34s footprint=%7.1f MiB  compressed=%7.1f MiB  resident=%7.1f MiB\n",
           label, s.footprint / 1048576.0, s.compressed / 1048576.0,
           s.resident / 1048576.0);
    fflush(stdout);
}

// Resident host pages in [p, p+len) per mincore.
static size_t resident_pages(void *p, size_t len) {
    size_t pages = len / (size_t)getpagesize();
    char *vec = malloc(pages);
    if (mincore(p, len, vec) != 0) {
        perror("mincore");
        free(vec);
        return (size_t)-1;
    }
    size_t n = 0;
    for (size_t i = 0; i < pages; i++)
        if (vec[i] & MINCORE_INCORE)
            n++;
    free(vec);
    return n;
}

static void report_residency(const char *label, unsigned char *base) {
    size_t page = (size_t)getpagesize();
    printf("%-34s residency/quarter (of %llu pages):", label,
           (unsigned long long)(QUARTER / page));
    for (int q = 0; q < 4; q++)
        printf(" Q%d=%zu", q + 1, resident_pages(base + (size_t)q * QUARTER, QUARTER));
    printf("\n");
    fflush(stdout);
}

static int advise(unsigned char *base, int q, int adv, const char *name) {
    uint64_t t0 = now_ns();
    int rc = madvise(base + (size_t)q * QUARTER, QUARTER, adv);
    uint64_t dt = now_ns() - t0;
    printf("  Q%d madvise(%s) -> %s (%.2f ms)\n", q + 1, name,
           rc == 0 ? "OK" : strerror(errno), dt / 1e6);
    fflush(stdout);
    return rc;
}

// Mode "race": does MADV_FREE_REUSABLE issued WHILE the pageout scan is actively
// compressing the range strand slots (the xnu vmp_laundry/vmp_cleaning skip), and does a
// second pass after the dust settles recover them (the janitor question)? We race one
// REUSABLE against every ballast step while compression is in flight, free the ballast
// (taking its own compressed share with it), then measure the leftover compressed
// attributable to the target and whether one more REUSABLE clears it.
static int race_mode(unsigned char *target) {
    memset(target, PATTERN, TARGET_SIZE);
    print_snap("after dirtying 2 GiB", take_snap());

    void *ballast[BALLAST_MAX];
    size_t nballast = 0;
    for (; nballast < BALLAST_MAX; nballast++) {
        void *b = mmap(NULL, BALLAST_CHUNK, PROT_READ | PROT_WRITE,
                       MAP_ANON | MAP_PRIVATE, -1, 0);
        if (b == MAP_FAILED)
            break;
        memset(b, 0xa5, BALLAST_CHUNK);
        ballast[nballast] = b;
        // Race: madvise the whole target while the scan may be laundering it.
        madvise(target, TARGET_SIZE, MADV_FREE_REUSABLE);
        size_t res = resident_pages(target, TARGET_SIZE);
        printf("  ballast %2zu GiB + REUSABLE race: target resident %6zu pages, "
               "task compressed %7.1f MiB\n",
               nballast + 1, res, take_snap().compressed / 1048576.0);
        if (res * (size_t)getpagesize() < TARGET_SIZE / 8)
            break;
    }
    for (size_t i = 0; i < nballast; i++)
        munmap(ballast[i], BALLAST_CHUNK);
    sleep(2); // let the compressor/scan settle

    struct snap s1 = take_snap();
    print_snap("settled (ballast gone)", s1);
    printf("leftover compressed attributable to target: %.1f MiB\n",
           s1.compressed / 1048576.0);

    uint64_t t0 = now_ns();
    int rc = madvise(target, TARGET_SIZE, MADV_FREE_REUSABLE);
    printf("janitor pass: madvise(FREE_REUSABLE) -> %s (%.2f ms)\n",
           rc == 0 ? "OK" : strerror(errno), (now_ns() - t0) / 1e6);
    struct snap s2 = take_snap();
    print_snap("after janitor pass", s2);
    printf("janitor recovered: compressed %+.1f MiB, footprint %+.1f MiB\n",
           ((double)s2.compressed - s1.compressed) / 1048576.0,
           ((double)s2.footprint - s1.footprint) / 1048576.0);
    return 0;
}

int main(int argc, char **argv) {
    // Mode: private (MAP_ANON|MAP_PRIVATE, the v1 run) or shared (MAP_ANON|MAP_SHARED —
    // the worker's guest RAM shows SM=SHM in vmmap, and xnu's reuse kill has silent no-op
    // conditions that depend on the object shape), or race (see race_mode).
    int shared = argc > 1 && strcmp(argv[1], "shared") == 0;
    printf("host page size: %d, mode: %s\n", getpagesize(),
           shared ? "MAP_SHARED" : "MAP_PRIVATE");

    unsigned char *target =
        mmap(NULL, TARGET_SIZE, PROT_READ | PROT_WRITE,
             MAP_ANON | (shared ? MAP_SHARED : MAP_PRIVATE), -1, 0);
    if (target == MAP_FAILED) {
        perror("mmap target");
        return 1;
    }
    if (argc > 1 && strcmp(argv[1], "race") == 0)
        return race_mode(target);

    print_snap("baseline", take_snap());
    memset(target, PATTERN, TARGET_SIZE);
    struct snap dirty = take_snap();
    print_snap("after dirtying 2 GiB", dirty);

    // ---- push the target into the compressor ----
    uint64_t t0 = now_ns();
    int po = madvise(target, TARGET_SIZE, MADV_PAGEOUT);
    printf("madvise(MADV_PAGEOUT) -> %s (%.2f ms)\n",
           po == 0 ? "OK" : strerror(errno), (now_ns() - t0) / 1e6);

    void *ballast[BALLAST_MAX];
    size_t nballast = 0;
    if (po != 0 || resident_pages(target, TARGET_SIZE) * (size_t)getpagesize() >
                       TARGET_SIZE / 2) {
        printf("PAGEOUT insufficient; applying memory ballast (up to %llu GiB)\n",
               BALLAST_MAX);
        for (; nballast < BALLAST_MAX; nballast++) {
            void *b = mmap(NULL, BALLAST_CHUNK, PROT_READ | PROT_WRITE,
                           MAP_ANON | MAP_PRIVATE, -1, 0);
            if (b == MAP_FAILED)
                break;
            memset(b, 0xa5, BALLAST_CHUNK);
            ballast[nballast] = b;
            size_t res = resident_pages(target, TARGET_SIZE);
            printf("  ballast %zu GiB: target resident %zu pages (%.0f%%)\n",
                   nballast + 1, res,
                   100.0 * res * getpagesize() / TARGET_SIZE);
            if (res * (size_t)getpagesize() < TARGET_SIZE / 4)
                break;
        }
    }
    for (size_t i = 0; i < nballast; i++)
        munmap(ballast[i], BALLAST_CHUNK);

    struct snap comp = take_snap();
    print_snap("after compression forcing", comp);
    report_residency("pre-advice", target);
    if (comp.compressed < 256 * 1048576ULL) {
        printf("WARNING: <256 MiB compressed — compressor forcing failed, "
               "results below are about RESIDENT pages only\n");
    }

    // ---- the four recipes ----
    struct snap before, after;

    before = take_snap();
    advise(target, 0, MADV_FREE_REUSABLE, "FREE_REUSABLE");
    after = take_snap();
    printf("  Q1 delta: footprint %+.1f MiB, compressed %+.1f MiB\n",
           ((double)after.footprint - before.footprint) / 1048576.0,
           ((double)after.compressed - before.compressed) / 1048576.0);

    before = after;
    advise(target, 1, MADV_ZERO, "ZERO");
    after = take_snap();
    printf("  Q2 delta: footprint %+.1f MiB, compressed %+.1f MiB\n",
           ((double)after.footprint - before.footprint) / 1048576.0,
           ((double)after.compressed - before.compressed) / 1048576.0);

    before = after;
    advise(target, 2, MADV_ZERO, "ZERO");
    advise(target, 2, MADV_FREE_REUSABLE, "FREE_REUSABLE");
    after = take_snap();
    printf("  Q3 delta: footprint %+.1f MiB, compressed %+.1f MiB\n",
           ((double)after.footprint - before.footprint) / 1048576.0,
           ((double)after.compressed - before.compressed) / 1048576.0);

    before = after;
    advise(target, 3, MADV_FREE_REUSABLE, "FREE_REUSABLE");
    advise(target, 3, MADV_ZERO, "ZERO");
    after = take_snap();
    printf("  Q4 delta: footprint %+.1f MiB, compressed %+.1f MiB\n",
           ((double)after.footprint - before.footprint) / 1048576.0,
           ((double)after.compressed - before.compressed) / 1048576.0);

    report_residency("post-advice", target);
    print_snap("post-advice", take_snap());

    // ---- content check: one read per 64 MiB per quarter ----
    for (int q = 0; q < 4; q++) {
        size_t pattern = 0, zero = 0;
        for (size_t off = 0; off < QUARTER; off += 64 * 1048576) {
            unsigned char v = target[(size_t)q * QUARTER + off];
            if (v == PATTERN)
                pattern++;
            else if (v == 0)
                zero++;
        }
        printf("Q%d content probe: %zu pattern, %zu zero (of %llu reads)\n", q + 1,
               pattern, zero, (unsigned long long)(QUARTER / (64 * 1048576ULL)));
    }
    struct snap end = take_snap();
    print_snap("after content probes", end);
    printf("\nverdict inputs: Q1-vs-Q2 compressed delta is the answer; a quarter "
           "whose compressed share survived its advice still bills the task.\n");
    return 0;
}
