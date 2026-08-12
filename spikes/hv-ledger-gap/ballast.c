/* ballast — deterministic host memory pressure for the S9 ledger-churn bench.
 *
 * Allocates <mib> MiB of INCOMPRESSIBLE private anon (xorshift fill, so the
 * compressor gets real byte pressure, not the ~free absorption a memset fill
 * gives it), in 256 MiB steps, printing "grown <mib-so-far>" after each step
 * (line-buffered — the harness waits for the target line). Then holds until
 * stdin closes or SIGTERM, prints "released", and exits — freeing everything
 * at once. Exits 0 on a clean release.
 *
 * Usage: ballast <mib>
 * The harness should treat an early exit (jetsam) as "pressure achieved,
 * partially" — the point is what the VM worker's ledger shows, not the
 * ballast's fate. See churn-probe.c for the fill rationale.
 */

#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

#define STEP_MIB 256UL

static volatile sig_atomic_t stop;

static void
on_term(int sig)
{
    (void)sig;
    stop = 1;
}

int
main(int argc, char **argv)
{
    if (argc != 2) {
        fprintf(stderr, "usage: %s <mib>\n", argv[0]);
        return 2;
    }
    size_t target_mib = (size_t)strtoul(argv[1], NULL, 10);
    /* sigaction with sa_flags=0: BSD signal() sets SA_RESTART, which would
     * keep the hold-loop's blocking read() alive through SIGTERM. */
    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_handler = on_term;
    sigaction(SIGTERM, &sa, NULL);
    sigaction(SIGINT, &sa, NULL);
    setvbuf(stdout, NULL, _IOLBF, 0);

    size_t grown = 0;
    while (grown < target_mib && !stop) {
        size_t step = target_mib - grown < STEP_MIB ? target_mib - grown
                                                    : STEP_MIB;
        size_t len = step << 20;
        uint64_t *b = mmap(NULL, len, PROT_READ | PROT_WRITE,
                           MAP_ANON | MAP_PRIVATE, -1, 0);
        if (b == MAP_FAILED) {
            printf("mmap-failed at %zu MiB\n", grown);
            break;
        }
        uint64_t x = (uint64_t)(uintptr_t)b | 1;
        for (size_t i = 0; i < len / 8; i++) {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            b[i] = x;
        }
        grown += step;
        printf("grown %zu\n", grown);
    }
    printf("holding %zu\n", grown);

    char c;
    while (!stop && read(STDIN_FILENO, &c, 1) > 0)
        ;
    printf("released\n");
    return 0;
}
