// How does DNSServiceQueryRecord (mDNSResponder, the host resolver that honours VPN split DNS)
// report CNAME chains, missing names, empty answers and batches? limina-netd builds the guest's
// DNS responses from these callbacks. See RESULTS.md.
//
//   probe [seconds-per-query]     (xcrun clang -o probe probe.c)
#include <dns_sd.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>

static double t0;
static double now_ms(void) { return clock_gettime_nsec_np(CLOCK_UPTIME_RAW) / 1e6; }

static void reply(DNSServiceRef ref, DNSServiceFlags flags, uint32_t ifi, DNSServiceErrorType err,
                  const char *fullname, uint16_t type, uint16_t cls, uint16_t rdlen,
                  const void *rdata, uint32_t ttl, void *ctx) {
    (void)ref; (void)cls; (void)rdata; (void)ctx;
    printf("  %7.1f ms  flags 0x%05x%s%s  if %u  err %d  %s type %u rdlen %u ttl %u\n",
           now_ms() - t0, flags, flags & kDNSServiceFlagsMoreComing ? " MORE" : "",
           flags & kDNSServiceFlagsAdd ? " ADD" : "", ifi, err, fullname, type, rdlen, ttl);
}

static void ask(const char *name, uint16_t type, DNSServiceFlags flags, int secs) {
    DNSServiceRef ref;
    printf("%s type %u flags 0x%x\n", name, type, flags);
    t0 = now_ms();
    DNSServiceErrorType e =
        DNSServiceQueryRecord(&ref, flags, 0, name, type, kDNSServiceClass_IN, reply, NULL);
    if (e) { printf("  start error %d\n", e); return; }
    struct pollfd p = {.fd = DNSServiceRefSockFD(ref), .events = POLLIN};
    while (now_ms() - t0 < secs * 1000.0) {
        int left = (int)(secs * 1000.0 - (now_ms() - t0));
        if (poll(&p, 1, left) > 0) {
            e = DNSServiceProcessResult(ref);
            if (e) { printf("  process error %d\n", e); break; }
        }
    }
    DNSServiceRefDeallocate(ref);
}

int main(int argc, char **argv) {
    setvbuf(stdout, 0, _IONBF, 0);
    int secs = argc > 1 ? atoi(argv[1]) : 3;
    struct { const char *n; uint16_t t; } q[] = {
        {"www.github.com", kDNSServiceType_A},       // CNAME to github.com
        {"github.com", kDNSServiceType_AAAA},        // exists, no AAAA (NODATA)
        {"no-such-name.limina.invalid", kDNSServiceType_A}, // NXDOMAIN
        {"gmail.com", kDNSServiceType_MX},
        {"_xmpp-server._tcp.gmail.com", kDNSServiceType_SRV},
        {"google.com", kDNSServiceType_TXT},
    };
    DNSServiceFlags modes[] = {0, kDNSServiceFlagsReturnIntermediates | kDNSServiceFlagsTimeout};
    for (unsigned m = 0; m < 2; m++)
        for (unsigned i = 0; i < sizeof q / sizeof q[0]; i++) ask(q[i].n, q[i].t, modes[m], secs);
    return 0;
}
