// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/*
 * hv-stage2-write-loss host driver.
 *
 * Question: does a stage-2 mapping change (hv_vm_protect / unmap+map) on guest
 * RAM pages the guest has JUST written discard those writes?  Observed in
 * limina as VP9 keyframes arriving all-zero at the host while the guest saw its
 * own bytes right after the memcpy.
 *
 * Protocol (see payload.S): the guest fills DATA with a salted pattern, the
 * host counts mismatches (host view), applies --op, counts again, restores RWX,
 * and the guest recounts (guest view) before refilling with a second salt.
 *
 * Options:
 *   --granule 4k|16k      stage-2 IPA granule (default: host default)
 *   --op protect|remap|none|reuse   the stage-2 change at CKPT1 (default protect)
 *   --touch none|read|write         host access to DATA before the op
 *   --shared              back RAM with MAP_SHARED instead of MAP_PRIVATE
 *   --pretouch-host       host writes DATA to zero before the guest runs
 *   --mmu                 guest enables an identity MMU map (RAM Normal write-back cacheable)
 *   --zva                 guest DC ZVAs DATA before filling (init_on_alloc's clear_page)
 *   --race                a host thread toggles DATA R|X <-> RWX while the guest refills it
 *   --race-once <ns>      a host thread protects DATA R|X exactly once, <ns> after the guest
 *                         starts a fill (CKPT0); faults heal the 16 KiB page and retry. Reports,
 *                         per fill, whether the protect landed inside the fill, the first healed
 *                         offset and the first mismatching one — a store lost right before the
 *                         permission change shows up as first_off just below the first heal.
 *   --fill simd|gpr       guest store shape: STR Qn (glibc memcpy) or STP Xn,Xm (default)
 *
 * Build/run/sign: build.sh (needs com.apple.security.hypervisor).
 */

#include <Hypervisor/Hypervisor.h>

#include <dlfcn.h>
#include <pthread.h>
#include <stdatomic.h>
#include <errno.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

#define RAM_BASE 0x80000000ULL
#define RAM_SIZE (64ULL << 20)
#define DATA_GPA 0x80100000ULL
#define DATA_LEN 0x40000ULL

#define MMIO_BASE 0x10000000ULL
#define M_CKPT1 0x00
#define M_CKPT2 0x08
#define M_CKPT3 0x10
#define M_CKPT0 0x18
#define M_DONE 0x20

#define BOOT_CPSR 0x3C5ULL /* EL1h, DAIF masked */

static const char *hv_err_name(hv_return_t r) {
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

static uint8_t *g_ram;
static void *gpa_to_hva(uint64_t gpa) { return g_ram + (gpa - RAM_BASE); }

static const char *g_op = "protect";
static const char *g_touch = "none";
static const char *g_granule = NULL;
static bool g_shared = false;
static bool g_pretouch_host = false;
static bool g_mmu = false;
static bool g_zva = false;
static bool g_race = false;
static bool g_simd = false;
static _Atomic bool g_race_on;
static _Atomic uint64_t g_race_toggles;

/* --race-once state: the vCPU thread arms a fill (CKPT0), the protect thread fires once. */
static int64_t g_once_ns = -1;
static _Atomic uint64_t g_once_gen;      /* bumped by the vCPU thread at each CKPT0 */
static _Atomic uint64_t g_once_deadline; /* CLOCK_UPTIME_RAW ns at which to protect */
static _Atomic uint64_t g_once_fired_at; /* 0 until the protect returned */
static _Atomic bool g_once_quit;
static uint64_t g_heals, g_first_heal_pa, g_first_heal_pc;

static uint64_t now_ns(void) { return clock_gettime_nsec_np(CLOCK_UPTIME_RAW); }

static void *once_main(void *arg) {
    (void)arg;
    uint64_t seen = 0;
    while (!atomic_load(&g_once_quit)) {
        uint64_t gen = atomic_load(&g_once_gen);
        if (gen == seen) continue;
        seen = gen;
        uint64_t deadline = atomic_load(&g_once_deadline);
        while (now_ns() < deadline) {}
        hv_vm_protect(DATA_GPA, DATA_LEN, HV_MEMORY_READ | HV_MEMORY_EXEC);
        atomic_store(&g_once_fired_at, now_ns());
    }
    return NULL;
}

/* --race: flip DATA between R|X and RWX as fast as possible while the guest refills it. */
static void *race_main(void *arg) {
    (void)arg;
    while (atomic_load(&g_race_on)) {
        hv_vm_protect(DATA_GPA, DATA_LEN, HV_MEMORY_READ | HV_MEMORY_EXEC);
        hv_vm_protect(DATA_GPA, DATA_LEN, HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC);
        atomic_fetch_add(&g_race_toggles, 1);
    }
    return NULL;
}

/* Count words not matching the salted pattern; report zero words and the first
 * mismatching offset. */
static uint64_t host_count(const char *tag, uint64_t salt, uint64_t *first_out) {
    volatile uint64_t *p = gpa_to_hva(DATA_GPA);
    uint64_t mism = 0, zeros = 0, first = UINT64_MAX;
    for (uint64_t off = 0; off < DATA_LEN; off += 16) {
        uint64_t a = off | (salt << 32), b = ~a;
        uint64_t va = p[off / 8], vb = p[off / 8 + 1];
        if (va != a) { mism++; if (first == UINT64_MAX) first = off; if (va == 0) zeros++; }
        if (vb != b) { mism++; if (first == UINT64_MAX) first = off + 8; if (vb == 0) zeros++; }
    }
    if (tag)
        printf("[host] %-28s salt=%llu mismatching words=%llu (zero=%llu) first_off=%s0x%llx\n", tag,
               salt, mism, zeros, first == UINT64_MAX ? "-" : "", first == UINT64_MAX ? 0 : first);
    if (first_out) *first_out = first;
    return mism;
}

/* --race-once: a fill just ended. Wait for the one-shot protect if it has not fired yet, restore
 * RWX, and print one line per fill: whether the protect landed inside the fill, how many stores
 * healed, and where the first heal and the first mismatch sit. */
static uint64_t g_fill_started_ns;
static void once_fill_done(uint64_t salt) {
    uint64_t end = now_ns();
    while (atomic_load(&g_once_fired_at) == 0) {}
    uint64_t fired = atomic_load(&g_once_fired_at);
    CHECK(hv_vm_protect(DATA_GPA, DATA_LEN, HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC));
    uint64_t first;
    uint64_t mism = host_count(NULL, salt, &first);
    printf("[once] salt=%llu delay_ns=%lld fill_us=%.1f protect=%s heals=%llu first_heal_off=%s0x%llx "
           "first_heal_pc=0x%llx mismatching=%llu first_mism_off=%s0x%llx\n",
           salt, (long long)g_once_ns, (end - g_fill_started_ns) / 1000.0,
           fired < end ? "inside" : "after", g_heals,
           g_heals ? "" : "-", g_heals ? g_first_heal_pa - DATA_GPA : 0, g_first_heal_pc, mism,
           first == UINT64_MAX ? "-" : "", first == UINT64_MAX ? 0 : first);
}

static void apply_op(void) {
    if (!strcmp(g_touch, "read")) {
        volatile uint8_t *p = gpa_to_hva(DATA_GPA);
        uint64_t s = 0;
        for (uint64_t off = 0; off < DATA_LEN; off += 4096) s += p[off];
        printf("[host] touched (read) DATA, sum=%llu\n", s);
    } else if (!strcmp(g_touch, "write")) {
        volatile uint8_t *p = gpa_to_hva(DATA_GPA);
        for (uint64_t off = 0; off < DATA_LEN; off += 4096) p[off] = p[off];
        printf("[host] touched (write same value) DATA\n");
    }
    if (!strcmp(g_op, "protect")) {
        CHECK(hv_vm_protect(DATA_GPA, DATA_LEN, HV_MEMORY_READ | HV_MEMORY_EXEC));
        printf("[host] hv_vm_protect(DATA, R|X)\n");
    } else if (!strcmp(g_op, "remap")) {
        CHECK(hv_vm_unmap(DATA_GPA, DATA_LEN));
        CHECK(hv_vm_map(gpa_to_hva(DATA_GPA), DATA_GPA, DATA_LEN,
                        HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC));
        printf("[host] hv_vm_unmap + hv_vm_map(DATA, RWX)\n");
    } else if (!strcmp(g_op, "reuse")) {
        int rc = madvise(gpa_to_hva(DATA_GPA), DATA_LEN, MADV_FREE_REUSE);
        printf("[host] madvise(DATA, MADV_FREE_REUSE) rc=%d errno=%d\n", rc, rc ? errno : 0);
    } else {
        printf("[host] no op\n");
    }
}

static void restore_rwx(void) {
    if (!strcmp(g_op, "protect"))
        CHECK(hv_vm_protect(DATA_GPA, DATA_LEN, HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC));
}

int main(int argc, char **argv) {
    const char *payload_path = "payload.bin";
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--granule") && i + 1 < argc) g_granule = argv[++i];
        else if (!strcmp(argv[i], "--op") && i + 1 < argc) g_op = argv[++i];
        else if (!strcmp(argv[i], "--touch") && i + 1 < argc) g_touch = argv[++i];
        else if (!strcmp(argv[i], "--shared")) g_shared = true;
        else if (!strcmp(argv[i], "--pretouch-host")) g_pretouch_host = true;
        else if (!strcmp(argv[i], "--mmu")) g_mmu = true;
        else if (!strcmp(argv[i], "--zva")) g_zva = true;
        else if (!strcmp(argv[i], "--race")) g_race = true;
        else if (!strcmp(argv[i], "--race-once") && i + 1 < argc) { g_once_ns = atoll(argv[++i]); g_op = "none"; }
        else if (!strcmp(argv[i], "--fill") && i + 1 < argc) g_simd = !strcmp(argv[++i], "simd");
        else payload_path = argv[i];
    }

    FILE *f = fopen(payload_path, "rb");
    if (!f) { perror(payload_path); return 1; }
    static uint8_t payload[1 << 20];
    size_t payload_len = fread(payload, 1, sizeof(payload), f);
    fclose(f);

    hv_vm_config_t config = NULL;
    if (g_granule) {
        typedef hv_vm_config_t (*create_fn)(void);
        typedef hv_return_t (*granule_fn)(hv_vm_config_t, uint32_t);
        create_fn create = (create_fn)dlsym(RTLD_DEFAULT, "hv_vm_config_create");
        granule_fn set = (granule_fn)dlsym(RTLD_DEFAULT, "hv_vm_config_set_ipa_granule");
        if (!create || !set) { fprintf(stderr, "no ipa granule API on this macOS\n"); return 1; }
        config = create();
        uint32_t g = !strcmp(g_granule, "4k") ? 0 : 1;
        CHECK(set(config, g));
    }
    CHECK(hv_vm_create(config));

    g_ram = mmap(NULL, RAM_SIZE, PROT_READ | PROT_WRITE,
                 MAP_ANON | (g_shared ? MAP_SHARED : MAP_PRIVATE), -1, 0);
    if (g_ram == MAP_FAILED) { perror("mmap"); return 1; }
    memcpy(g_ram, payload, payload_len);
    if (g_pretouch_host) memset(gpa_to_hva(DATA_GPA), 0, DATA_LEN);
    *(uint64_t *)gpa_to_hva(RAM_BASE + 0x2000) = (g_mmu ? 1 : 0) | (g_zva ? 2 : 0) | (g_simd ? 4 : 0);

    CHECK(hv_vm_map(g_ram, RAM_BASE, RAM_SIZE, HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC));
    printf("config: granule=%s op=%s touch=%s shared=%d pretouch-host=%d mmu=%d zva=%d fill=%s "
           "race-once=%lld payload=%zuB\n",
           g_granule ? g_granule : "default", g_op, g_touch, g_shared, g_pretouch_host, g_mmu, g_zva,
           g_simd ? "simd" : "gpr", (long long)g_once_ns, payload_len);

    pthread_t once_thread;
    if (g_once_ns >= 0) pthread_create(&once_thread, NULL, once_main, NULL);

    hv_vcpu_t vcpu;
    hv_vcpu_exit_t *vexit;
    CHECK(hv_vcpu_create(&vcpu, &vexit, NULL));
    CHECK(hv_vcpu_set_reg(vcpu, HV_REG_CPSR, BOOT_CPSR));
    CHECK(hv_vcpu_set_reg(vcpu, HV_REG_PC, RAM_BASE));

    uint64_t guest_mism = UINT64_MAX;
    bool done = false;
    int exits = 0;
    while (!done && exits < 100000) {
        CHECK(hv_vcpu_run(vcpu));
        exits++;
        if (vexit->reason != HV_EXIT_REASON_EXCEPTION) {
            fprintf(stderr, "unexpected exit reason %u\n", vexit->reason);
            return 1;
        }
        uint64_t syn = vexit->exception.syndrome;
        uint64_t ec = (syn >> 26) & 0x3f;
        uint64_t pa = vexit->exception.physical_address;
        if (ec != 0x24) {
            uint64_t pc = 0;
            hv_vcpu_get_reg(vcpu, HV_REG_PC, &pc);
            fprintf(stderr, "unhandled exception ec=0x%llx syndrome=0x%llx pc=0x%llx pa=0x%llx\n",
                    ec, syn, pc, pa);
            return 1;
        }
        bool iswrite = (syn >> 6) & 1;
        uint32_t srt = (syn >> 16) & 0x1f;
        if (pa >= RAM_BASE && pa < RAM_BASE + RAM_SIZE) {
            /* A write into protected DATA after the op: should not happen (we
             * restore RWX before resuming). Report and heal. */
            uint64_t pc = 0;
            hv_vcpu_get_reg(vcpu, HV_REG_PC, &pc);
            static int nheal;
            if (g_heals++ == 0) { g_first_heal_pa = pa; g_first_heal_pc = pc; }
            if ((++nheal <= 3 || !g_race) && g_once_ns < 0)
                printf("[exit] RAM data abort pa=0x%llx wnr=%d xfsc=0x%llx pc=0x%llx -> restore RWX, retry\n",
                       pa, iswrite, syn & 0x3f, pc);
            CHECK(hv_vm_protect(pa & ~0x3fffULL, 0x4000, HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC));
            continue;
        }
        uint64_t pc = 0;
        CHECK(hv_vcpu_get_reg(vcpu, HV_REG_PC, &pc));
        CHECK(hv_vcpu_set_reg(vcpu, HV_REG_PC, pc + 4));
        if (!iswrite) {
            if (srt < 31) CHECK(hv_vcpu_set_reg(vcpu, HV_REG_X0 + srt, 0));
            continue;
        }
        uint64_t val = 0;
        if (srt < 31) CHECK(hv_vcpu_get_reg(vcpu, HV_REG_X0 + srt, &val));
        switch (pa - MMIO_BASE) {
        case M_CKPT0:
            if (g_once_ns >= 0) {
                g_heals = 0;
                g_first_heal_pa = g_first_heal_pc = 0;
                atomic_store(&g_once_fired_at, 0);
                g_fill_started_ns = now_ns();
                atomic_store(&g_once_deadline, g_fill_started_ns + (uint64_t)g_once_ns);
                atomic_fetch_add(&g_once_gen, 1);
            }
            break;
        case M_CKPT1:
            printf("[ckpt1] guest filled DATA with salt %llu\n", val);
            if (g_once_ns >= 0) { once_fill_done(1); break; }
            host_count("before op", 1, NULL);
            apply_op();
            host_count("after op", 1, NULL);
            restore_rwx();
            host_count("after restore", 1, NULL);
            if (g_race) {
                atomic_store(&g_race_on, true);
                pthread_t t;
                pthread_create(&t, NULL, race_main, NULL);
                printf("[host] race thread started\n");
            }
            break;
        case M_CKPT2:
            guest_mism = val;
            printf("[ckpt2] guest recount: mismatching words=%llu\n", val);
            break;
        case M_CKPT3:
            if (g_once_ns >= 0) { once_fill_done(2); break; }
            if (g_race) {
                atomic_store(&g_race_on, false);
                usleep(2000);
                CHECK(hv_vm_protect(DATA_GPA, DATA_LEN, HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC));
                printf("[host] race thread stopped after %llu toggles\n", (unsigned long long)atomic_load(&g_race_toggles));
            }
            printf("[ckpt3] guest refilled DATA with salt %llu\n", val);
            host_count("after refill", 2, NULL);
            break;
        case M_DONE:
            done = true;
            break;
        default:
            printf("[exit] unexpected MMIO write pa=0x%llx\n", pa);
        }
    }
    printf("exits=%d done=%d guest_mismatches=%llu\n", exits, done, guest_mism);
    (void)g_race_toggles;
    if (g_once_ns >= 0) { atomic_store(&g_once_quit, true); pthread_join(once_thread, NULL); }
    CHECK(hv_vcpu_destroy(vcpu));
    hv_vm_unmap(RAM_BASE, RAM_SIZE);
    CHECK(hv_vm_destroy());
    return done ? 0 : 1;
}
