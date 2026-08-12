#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>
#include <stdint.h>
#include <sys/types.h>
#define LEDGER_TEMPLATE_INFO 2
#define LEDGER_ENTRY_INFO 1
struct lti { char n[32], g[32], u[32]; };
struct lei { int64_t bal, cred, deb; uint64_t a,b,c; };
extern int ledger(int cmd, caddr_t a1, caddr_t a2, caddr_t a3);
static void led(const char *tag) {
    static struct lti t[256]; static struct lei e[256];
    int tl=256, el=256;
    ledger(LEDGER_TEMPLATE_INFO,(caddr_t)t,(caddr_t)&tl,NULL);
    ledger(LEDGER_ENTRY_INFO,(caddr_t)(intptr_t)getpid(),(caddr_t)e,(caddr_t)&el);
    for (int i=0;i<el && i<tl;i++)
        if (!strcmp(t[i].n,"phys_footprint")||!strcmp(t[i].n,"reusable")||!strcmp(t[i].n,"internal"))
            printf("  %s %s=%.3fG(cred %.3fG)", tag, t[i].n, e[i].bal/1073741824.0, e[i].cred/1073741824.0);
    printf("\n");
}
int main(int argc, char **argv) {
    int nores = argc>1 && atoi(argv[1]);
    size_t len = 1UL<<30;
    void *p = mmap(NULL,len,PROT_READ|PROT_WRITE,MAP_ANON|MAP_PRIVATE|(nores?MAP_NORESERVE:0),-1,0);
    if (p==MAP_FAILED){perror("mmap");return 1;}
    printf("flags: MAP_PRIVATE%s\n", nores?"|MAP_NORESERVE":"");
    memset(p,0x5A,len); led("dirtied");
    int rc = madvise(p,len,MADV_FREE_REUSABLE);
    printf("madvise rc=%d\n",rc); led("post-madv");
    return 0;
}
