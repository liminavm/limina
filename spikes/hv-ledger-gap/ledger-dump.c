/* ledger-dump — itemize a task's kernel ledger entries by name.
 *
 * The phys_footprint "ledger gap" (2026-08-10, dogfood Mac): a 24 GiB-guest HVF worker
 * showed phys_footprint 49.2 G while footprint(1)/vmmap could itemize only ~26 G.
 * The missing ~23 G is charged to the task by a ledger entry no userland tool
 * itemizes — suspected to be the hv-mapped guest-RAM charge. This tool dumps the
 * kernel's own per-task ledger (every entry: balance/credit/debit, by template
 * name) so the bucket can be named directly instead of inferred by subtraction.
 *
 * The ledger(2) syscall is private but stable (bsd/kern/kern_ledger.c in xnu);
 * struct layouts below match xnu's <kern/ledger.h> userland-visible structs.
 * Run as the task's owner (or root). Usage: ledger-dump <pid> [-a]
 *   -a  print all entries, including zero balances
 */

#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
#include <unistd.h>

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

static double
gib(int64_t v)
{
    return (double)v / 1073741824.0;
}

int
main(int argc, char **argv)
{
    if (argc < 2) {
        fprintf(stderr, "usage: %s <pid> [-a]\n", argv[0]);
        return 2;
    }
    pid_t pid = (pid_t)atoi(argv[1]);
    int all = argc > 2 && strcmp(argv[2], "-a") == 0;

    struct ledger_info info;
    memset(&info, 0, sizeof(info));
    if (ledger(LEDGER_INFO, (caddr_t)(intptr_t)pid, (caddr_t)&info, NULL) < 0) {
        fprintf(stderr, "ledger(LEDGER_INFO, pid %d): %s\n", pid, strerror(errno));
        return 1;
    }
    printf("task ledger '%s' id=%lld entries=%lld (pid %d)\n\n", info.li_name,
           (long long)info.li_id, (long long)info.li_entries, pid);

    int cap = (int)info.li_entries;
    if (cap < 1 || cap > 4096)
        cap = 1024;

    struct ledger_template_info *tpl = calloc((size_t)cap, sizeof(*tpl));
    struct ledger_entry_info *ent = calloc((size_t)cap, sizeof(*ent));
    if (tpl == NULL || ent == NULL) {
        fprintf(stderr, "out of memory\n");
        return 1;
    }

    int tlen = cap;
    if (ledger(LEDGER_TEMPLATE_INFO, (caddr_t)tpl, (caddr_t)&tlen, NULL) < 0) {
        fprintf(stderr, "ledger(LEDGER_TEMPLATE_INFO): %s\n", strerror(errno));
        return 1;
    }

    int elen = cap;
    if (ledger(LEDGER_ENTRY_INFO, (caddr_t)(intptr_t)pid, (caddr_t)ent,
               (caddr_t)&elen) < 0) {
        fprintf(stderr, "ledger(LEDGER_ENTRY_INFO, pid %d): %s\n", pid,
                strerror(errno));
        return 1;
    }

    int n = elen < tlen ? elen : tlen;
    printf("%-36s %-10s %-14s %16s %16s %16s\n", "entry", "group", "units",
           "balance", "credit", "debit");
    int64_t bytes_sum = 0;
    for (int i = 0; i < n; i++) {
        int is_bytes = strcmp(tpl[i].lti_units, "bytes") == 0;
        /* Sum only physmem-group entries: byte-unit entries also include I/O
         * counters (logical_writes etc.) that would swamp the memory total. */
        if (is_bytes && strcmp(tpl[i].lti_group, "physmem") == 0 &&
            strcmp(tpl[i].lti_name, "phys_footprint") != 0)
            bytes_sum += ent[i].lei_balance;
        if (!all && ent[i].lei_balance == 0 && ent[i].lei_credit == 0)
            continue;
        if (is_bytes)
            printf("%-36s %-10s %-14s %14.3f G %14.3f G %14.3f G\n",
                   tpl[i].lti_name, tpl[i].lti_group, tpl[i].lti_units,
                   gib(ent[i].lei_balance), gib(ent[i].lei_credit),
                   gib(ent[i].lei_debit));
        else
            printf("%-36s %-10s %-14s %16lld %16lld %16lld\n", tpl[i].lti_name,
                   tpl[i].lti_group, tpl[i].lti_units,
                   (long long)ent[i].lei_balance, (long long)ent[i].lei_credit,
                   (long long)ent[i].lei_debit);
    }
    printf("\nsum of physmem byte balances (excl. phys_footprint itself): %.3f G\n",
           gib(bytes_sum));
    printf("(entries reported: template %d, task %d)\n", tlen, elen);

    free(tpl);
    free(ent);
    return 0;
}
