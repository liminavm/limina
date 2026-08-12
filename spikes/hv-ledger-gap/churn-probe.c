/* churn-probe — reproduce the compressor-slot orphan leak (RESULTS.md round 3).
 *
 * The dogfood restart experiment showed a 24 GiB-guest worker billed ~45 G of
 * internal_compressed while only ~22 G was object-attributable; at task death
 * exactly the attributable part was freed and ~21 G of real compressed slots
 * stayed behind, owned by nobody ("Pages stored in compressor" 57.7 G vs 22.7 G
 * summed over every queryable task). Prime suspect: MADV_FREE_REUSABLE issued
 * against ranges whose pages are sitting in the compressor (the balloon FRQ
 * path, device.rs:340) — the natural control is Parallels, which churns the
 * compressor at the same scale with no REUSABLE and no pathology.
 *
 * This toy mimics the dogfood shape: anon MAP_PRIVATE|MAP_NORESERVE buffer
 * (vm-memory's from_ranges flags), optionally hv_vm_map'ed, dirtied, forced
 * into the compressor by a ballast child, optionally madvised REUSABLE, then
 * re-dirtied — for N cycles. Readouts per phase: own ledger (ledger(2)),
 * object-attributable compressed (mach_vm_page_range_query PAGED_OUT count),
 * and system-wide compressor stats. The sharpened signature is measured by the
 * DRIVER after this process exits: if system "stored" does not return to the
 * pre-run baseline, the orphan leak reproduced (residue ~ buffer x cycles).
 *
 * Usage: churn-probe [-g gib] [-c cycles] [-R] [-H] [-S] [-B ballast-cap-gib]
 *                    [-t frac] [-x] [-X] [-w secs]
 *   -R  madvise(MADV_FREE_REUSABLE) after each compression phase (the suspect)
 *   -H  hv_vm_map the buffer (needs com.apple.security.hypervisor codesign)
 *   -S  MAP_SHARED instead of the default MAP_PRIVATE
 *   -t  required compressed fraction to count a cycle as valid (default 0.5)
 *   -x  incompressible (xorshift) ballast fill — creates BYTE pressure in the
 *       compressor pool so old segments swap out (0x5A ballast compresses to
 *       ~nothing and only ever exercises resident-segment slots)
 *   -X  incompressible buffer fill (pair with -x for the swapped-slot leg)
 *   -w  after the compress gate, keep the ballast and wait up to <secs> for
 *       the buffer's segments to age to swap (segswap growth >= 1/8 of the
 *       buffer's expected segment count) before madvising — targets the
 *       freeing-a-slot-in-a-swapped-out-segment path where the dogfood
 *       residue lives
 * A cycle that never reaches -t is printed as VOID and is not evidence.
 */

#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/sysctl.h>
#include <sys/types.h>
#include <unistd.h>

#include <mach/mach.h>
#include <mach/mach_vm.h>

#include <Hypervisor/Hypervisor.h>

#ifndef VM_PAGE_QUERY_PAGE_PRESENT
#define VM_PAGE_QUERY_PAGE_PRESENT 0x1
#endif
#ifndef VM_PAGE_QUERY_PAGE_PAGED_OUT
#define VM_PAGE_QUERY_PAGE_PAGED_OUT 0x10
#endif

#define PAGE_SZ 16384UL
#define GIB (1024UL * 1024UL * 1024UL)

/* ---- ledger(2) self-read (see ledger-dump.c for the full story) ---- */

#define LEDGER_INFO 0
#define LEDGER_ENTRY_INFO 1
#define LEDGER_TEMPLATE_INFO 2
#define LEDGER_NAME_MAX 32

struct ledger_info {
    char li_name[LEDGER_NAME_MAX];
    int64_t li_id;
    int64_t li_entries;
};

struct ledger_template_info {
    char lti_name[LEDGER_NAME_MAX];
    char lti_group[LEDGER_NAME_MAX];
    char lti_units[LEDGER_NAME_MAX];
};

struct ledger_entry_info {
    int64_t lei_balance;
    int64_t lei_credit;
    int64_t lei_debit;
    uint64_t lei_limit;
    uint64_t lei_refill_period;
    uint64_t lei_last_refill;
};

extern int ledger(int cmd, caddr_t arg1, caddr_t arg2, caddr_t arg3);

struct self_ledger {
    int64_t internal;
    int64_t internal_compressed;
    int64_t reusable;
    int64_t reusable_credit;
    int64_t phys_footprint;
};

static int
read_self_ledger(struct self_ledger *out)
{
    static struct ledger_template_info tpl[256];
    static struct ledger_entry_info ent[256];
    int tlen = 256, elen = 256;
    pid_t pid = getpid();

    if (ledger(LEDGER_TEMPLATE_INFO, (caddr_t)tpl, (caddr_t)&tlen, NULL) < 0)
        return -1;
    if (ledger(LEDGER_ENTRY_INFO, (caddr_t)(intptr_t)pid, (caddr_t)ent,
               (caddr_t)&elen) < 0)
        return -1;
    int n = elen < tlen ? elen : tlen;
    memset(out, 0, sizeof(*out));
    for (int i = 0; i < n; i++) {
        if (strcmp(tpl[i].lti_name, "internal") == 0)
            out->internal = ent[i].lei_balance;
        else if (strcmp(tpl[i].lti_name, "internal_compressed") == 0)
            out->internal_compressed = ent[i].lei_balance;
        else if (strcmp(tpl[i].lti_name, "reusable") == 0) {
            out->reusable = ent[i].lei_balance;
            out->reusable_credit = ent[i].lei_credit;
        } else if (strcmp(tpl[i].lti_name, "phys_footprint") == 0)
            out->phys_footprint = ent[i].lei_balance;
    }
    return 0;
}

/* ---- buffer dispositions ---- */

static int *dispositions; /* one int per page, allocated once */

static void
buf_dispositions(uint8_t *buf, size_t len, uint64_t *resident, uint64_t *paged_out)
{
    mach_vm_size_t count = len / PAGE_SZ;
    kern_return_t kr = mach_vm_page_range_query(
        mach_task_self(), (mach_vm_offset_t)(uintptr_t)buf, len,
        (mach_vm_address_t)(uintptr_t)dispositions, &count);
    *resident = *paged_out = 0;
    if (kr != KERN_SUCCESS) {
        fprintf(stderr, "mach_vm_page_range_query: %s\n", mach_error_string(kr));
        return;
    }
    for (mach_vm_size_t i = 0; i < count; i++) {
        if (dispositions[i] & VM_PAGE_QUERY_PAGE_PRESENT)
            (*resident)++;
        if (dispositions[i] & VM_PAGE_QUERY_PAGE_PAGED_OUT)
            (*paged_out)++;
    }
}

/* ---- system-wide compressor state ---- */

struct sys_stats {
    uint64_t stored;   /* uncompressed-equivalent pages held by the compressor */
    uint64_t occupied; /* resident pages used by compressor segments */
    uint64_t free_pages;
    int64_t seg_total;
    int64_t seg_swappedout;
};

static void
sys_sample(struct sys_stats *out)
{
    vm_statistics64_data_t vs;
    mach_msg_type_number_t c = HOST_VM_INFO64_COUNT;
    memset(out, 0, sizeof(*out));
    if (host_statistics64(mach_host_self(), HOST_VM_INFO64, (host_info64_t)&vs,
                          &c) == KERN_SUCCESS) {
        out->stored = vs.total_uncompressed_pages_in_compressor;
        out->occupied = vs.compressor_page_count;
        out->free_pages = vs.free_count;
    }
    size_t sz = sizeof(out->seg_total);
    if (sysctlbyname("vm.compressor.segment.total", &out->seg_total, &sz, NULL,
                     0) != 0)
        out->seg_total = -1;
    sz = sizeof(out->seg_swappedout);
    if (sysctlbyname("vm.compressor.segment.swappedout", &out->seg_swappedout,
                     &sz, NULL, 0) != 0)
        out->seg_swappedout = -1;
}

static void
sample(const char *phase, int cycle, uint8_t *buf, size_t len)
{
    struct self_ledger l;
    struct sys_stats s;
    uint64_t res = 0, out = 0;

    if (read_self_ledger(&l) < 0)
        fprintf(stderr, "ledger self-read failed: %s\n", strerror(errno));
    buf_dispositions(buf, len, &res, &out);
    sys_sample(&s);
    printf("S cycle=%d phase=%-14s ic=%7.3fG int=%7.3fG reus=%7.3fG "
           "reus_cred=%8.3fG pf=%7.3fG buf_res=%llu buf_out=%llu "
           "stored=%llu occ=%llu segs=%lld segswap=%lld free=%llu\n",
           cycle, phase, (double)l.internal_compressed / GIB,
           (double)l.internal / GIB, (double)l.reusable / GIB,
           (double)l.reusable_credit / GIB, (double)l.phys_footprint / GIB,
           (unsigned long long)res, (unsigned long long)out,
           (unsigned long long)s.stored, (unsigned long long)s.occupied,
           (long long)s.seg_total, (long long)s.seg_swappedout,
           (unsigned long long)s.free_pages);
    fflush(stdout);
}

/* ---- ballast child: grows 1 GiB of dirty private anon per 'g' command,
 * drops it all on 'd'. MUST be forked BEFORE the buffer exists: a fork taken
 * after the parent dirties the buffer leaves the child holding a COW
 * reference to the buffer's anon object, and MADV_FREE_REUSABLE on a
 * COW-shared object silently no-ops (rc=0, nothing reclaimed) — the probe's
 * own self-test caught this. ---- */

static pid_t ballast_pid = -1;
static int ballast_cmd = -1, ballast_ack = -1;
static int ballast_random = 0; /* set before ballast_start() */

static void
fill_random(uint8_t *b, size_t len, uint64_t seed)
{
    uint64_t x = seed | 1;
    uint64_t *w = (uint64_t *)b;
    for (size_t i = 0; i < len / 8; i++) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        w[i] = x;
    }
}

static void
ballast_start(void)
{
    int p2c[2], c2p[2];
    if (pipe(p2c) < 0 || pipe(c2p) < 0) {
        perror("pipe");
        exit(1);
    }
    ballast_pid = fork();
    if (ballast_pid < 0) {
        perror("fork");
        exit(1);
    }
    if (ballast_pid == 0) {
        close(p2c[1]);
        close(c2p[0]);
        uint8_t *maps[64];
        int nmaps = 0;
        char cmd;
        while (read(p2c[0], &cmd, 1) == 1) {
            if (cmd == 'g' && nmaps < 64) {
                uint8_t *b = mmap(NULL, GIB, PROT_READ | PROT_WRITE,
                                  MAP_ANON | MAP_PRIVATE, -1, 0);
                if (b == MAP_FAILED)
                    break;
                /* Default: compressible-but-nonzero fill (gentle, exercises
                 * only resident segments). -x: incompressible, for byte
                 * pressure that forces segment swap-out. */
                if (ballast_random)
                    fill_random(b, GIB, (uint64_t)(uintptr_t)b + nmaps);
                else
                    memset(b, 0x5A, GIB);
                maps[nmaps++] = b;
            } else if (cmd == 'd') {
                while (nmaps > 0)
                    munmap(maps[--nmaps], GIB);
            } else {
                break;
            }
            char ack = 'k';
            if (write(c2p[1], &ack, 1) != 1)
                break;
        }
        _exit(0);
    }
    close(p2c[0]);
    close(c2p[1]);
    ballast_cmd = p2c[1];
    ballast_ack = c2p[0];
}

/* Returns 1 on ack, 0 if the child is gone (jetsam etc.). */
static int
ballast_send(char c)
{
    if (ballast_cmd < 0 || write(ballast_cmd, &c, 1) != 1)
        return 0;
    if (read(ballast_ack, &c, 1) != 1)
        return 0;
    return 1;
}

static void
ballast_stop(void)
{
    if (ballast_cmd >= 0) {
        close(ballast_cmd);
        close(ballast_ack);
        ballast_cmd = ballast_ack = -1;
    }
    if (ballast_pid > 0) {
        int st;
        kill(ballast_pid, SIGKILL);
        waitpid(ballast_pid, &st, 0);
        ballast_pid = -1;
    }
}

/* ---- phases ---- */

static int buffer_random = 0;

static void
dirty(uint8_t *buf, size_t len, int cycle)
{
    if (buffer_random) {
        fill_random(buf, len, (uint64_t)cycle * 0x9E3779B97F4A7C15ULL);
        return;
    }
    /* Distinct nonzero byte per page (zero pages are dropped, not stored;
     * identical pages are fine — WKdm does not dedupe across pages). */
    for (size_t off = 0; off < len; off += PAGE_SZ)
        memset(buf + off, (uint8_t)(((off / PAGE_SZ) + cycle * 37) % 255 + 1),
               PAGE_SZ);
}

int
main(int argc, char **argv)
{
    size_t gib = 4;
    int cycles = 3;
    int use_reusable = 0, use_hv = 0, use_shared = 0;
    size_t ballast_cap = 22;
    double target_frac = 0.5;
    int age_wait = 0;
    int opt;

    while ((opt = getopt(argc, argv, "g:c:RHSB:t:xXw:")) != -1) {
        switch (opt) {
        case 'g': gib = (size_t)atoi(optarg); break;
        case 'c': cycles = atoi(optarg); break;
        case 'R': use_reusable = 1; break;
        case 'H': use_hv = 1; break;
        case 'S': use_shared = 1; break;
        case 'B': ballast_cap = (size_t)atoi(optarg); break;
        case 't': target_frac = atof(optarg); break;
        case 'x': ballast_random = 1; break;
        case 'X': buffer_random = 1; break;
        case 'w': age_wait = atoi(optarg); break;
        default:
            fprintf(stderr, "usage: %s [-g gib] [-c cycles] [-R] [-H] [-S] "
                            "[-B ballast-cap-gib] [-t frac] [-x] [-X] "
                            "[-w secs]\n", argv[0]);
            return 2;
        }
    }

    signal(SIGPIPE, SIG_IGN);
    size_t len = gib * GIB;
    size_t pages = len / PAGE_SZ;
    dispositions = calloc(pages, sizeof(int));
    if (dispositions == NULL) {
        perror("calloc");
        return 1;
    }

    /* Fork the ballast helper BEFORE the buffer exists (see ballast_start). */
    ballast_start();

    int flags = MAP_ANON | MAP_NORESERVE |
                (use_shared ? MAP_SHARED : MAP_PRIVATE);
    uint8_t *buf = mmap(NULL, len, PROT_READ | PROT_WRITE, flags, -1, 0);
    if (buf == MAP_FAILED) {
        perror("mmap buffer");
        return 1;
    }

    if (use_hv) {
        hv_return_t hr = hv_vm_create(NULL);
        if (hr != HV_SUCCESS) {
            fprintf(stderr, "hv_vm_create: 0x%x (codesigned with the "
                            "hypervisor entitlement?)\n", hr);
            return 1;
        }
        hr = hv_vm_map(buf, 0x100000000ULL, len,
                       HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC);
        if (hr != HV_SUCCESS) {
            fprintf(stderr, "hv_vm_map: 0x%x\n", hr);
            return 1;
        }
    }

    printf("# churn-probe pid=%d buf=%zuGiB pages=%zu mode=%s%s%s%s%s cycles=%d "
           "ballast_cap=%zuGiB target_frac=%.2f age_wait=%ds\n",
           getpid(), gib, pages, use_shared ? "shared" : "private",
           use_hv ? "+hv" : "", use_reusable ? "+REUSABLE" : "",
           ballast_random ? "+xballast" : "", buffer_random ? "+Xbuf" : "",
           cycles, ballast_cap, target_frac, age_wait);
    sample("start", 0, buf, len);

    for (int cy = 1; cy <= cycles; cy++) {
        dirty(buf, len, cy);
        sample("dirtied", cy, buf, len);

        /* Force the buffer into the compressor: grow ballast until the
         * target fraction of buffer pages is PAGED_OUT. */
        uint64_t res, out;
        size_t grown = 0;
        int settle;
        for (;;) {
            buf_dispositions(buf, len, &res, &out);
            if ((double)out / (double)pages >= target_frac)
                break;
            if (grown >= ballast_cap || !ballast_send('g'))
                break;
            grown++;
            sleep(2);
        }
        /* Ballast at cap (or child dead): give the pageout thread a moment. */
        for (settle = 0; settle < 45; settle++) {
            buf_dispositions(buf, len, &res, &out);
            if ((double)out / (double)pages >= target_frac)
                break;
            sleep(2);
        }
        int valid = (double)out / (double)pages >= target_frac;
        printf("# cycle %d: ballast=%zuGiB compressed_frac=%.2f%s\n", cy,
               grown, (double)out / (double)pages, valid ? "" : "  VOID");
        sample("compressed", cy, buf, len);

        if (age_wait > 0) {
            /* Keep the ballast and wait for segment swap-out to reach the
             * buffer's segments (heuristic: segswap grows by >= 1/8 of the
             * buffer's expected segment population). */
            struct sys_stats s0, sn;
            sys_sample(&s0);
            int64_t want = (int64_t)(out * PAGE_SZ / (64 * 1024)) / 8;
            int waited;
            for (waited = 0; waited < age_wait; waited += 5) {
                sys_sample(&sn);
                if (sn.seg_swappedout - s0.seg_swappedout >= want)
                    break;
                sleep(5);
            }
            printf("# cycle %d: aged %ds, segswap +%lld (want +%lld)%s\n", cy,
                   waited, (long long)(sn.seg_swappedout - s0.seg_swappedout),
                   (long long)want,
                   sn.seg_swappedout - s0.seg_swappedout >= want
                       ? "" : "  AGE-SHORT");
            sample("aged", cy, buf, len);
        }

        if (use_reusable) {
            if (madvise(buf, len, MADV_FREE_REUSABLE) != 0)
                fprintf(stderr, "madvise(MADV_FREE_REUSABLE): %s\n",
                        strerror(errno));
            sample("reusable", cy, buf, len);
        }

        if (!ballast_send('d'))
            fprintf(stderr, "ballast child died during cycle %d\n", cy);
        sleep(3);
        sample("ballast-freed", cy, buf, len);
    }
    ballast_stop();

    /* Final re-dirty so the last cycle's REUSABLE range is re-populated the
     * way a guest re-touching ballooned-out pages would. */
    dirty(buf, len, cycles + 1);
    sample("final-redirty", cycles, buf, len);

    if (use_hv) {
        hv_vm_unmap(0x100000000ULL, len);
        hv_vm_destroy();
    }
    printf("# exiting; driver measures the post-exit system residue\n");
    return 0;
}
