// A minimal "guest" on a vmnet interface: does a VM attached this way get a usable network?
//
// probe.c proved that each mode starts and that one frame moves. This plays the guest's network
// stack by hand, raw frames over vmnet_read/vmnet_write, to check what a VM would actually get:
//
//   lease <shared|host|bridged> [ifname] [--vhdr]
//       DHCP DISCOVER/REQUEST -> lease (address, router, DNS, MTU); ARP the router; ICMP echo to
//       the router and, outside host mode, to 1.1.1.1 through it. --vhdr starts the interface with
//       vmnet_enable_virtio_header_key and reports the 12-byte header it sees on every frame.
//   ext <ifname|default|follow> --hold secs
//       A shared network whose NAT uplink is chosen with set_external_interface (default = vmnet's
//       own pick), checked once a second. follow tracks the host's route to 1.1.1.1 and rebuilds
//       the network on the new uplink when it changes, keeping the guest MAC.
//   netobj
//       macOS 26 network-object API: one shared network with a DHCP reservation and a port
//       forward, two interfaces on it (lease each, ARP each other), and an interface on a second
//       network that must not reach them. A host connect() to the forwarded port must arrive as a
//       SYN on the reserved interface.
//
// Build: see run.sh. Needs com.apple.security.virtualization (both.entitlements); runs non-root.
#include <arpa/inet.h>
#include <dispatch/dispatch.h>
#include <errno.h>
#include <fcntl.h>
#include <ifaddrs.h>
#include <net/ethernet.h>
#include <net/if.h>
#include <net/route.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <sys/uio.h>
#include <time.h>
#include <unistd.h>
#include <SystemConfiguration/SystemConfiguration.h>
#include <vmnet/vmnet.h>

#define BUF 70000

typedef struct {
    const char *name;
    interface_ref ifc;
    dispatch_queue_t q;
    uint8_t mac[6];
    int vhdr;                // frames carry a 12-byte virtio_net_hdr prefix
    uint64_t max_pkt;
    struct in_addr ip, mask, router, dns, server;
    uint32_t mtu;
    uint8_t router_mac[6];
    int seen_vhdr_nonzero;   // a received header with any non-zero byte
    int rx_frames, rx_big;   // frames read; frames larger than 1514 bytes
} nic;

static double now(void) {
    struct timeval tv;
    gettimeofday(&tv, NULL);
    return tv.tv_sec + tv.tv_usec / 1e6;
}

static uint16_t csum(const void *p, size_t n, uint32_t s) {
    const uint8_t *b = p;
    for (; n > 1; n -= 2, b += 2) s += (b[0] << 8) | b[1];
    if (n) s += b[0] << 8;
    while (s >> 16) s = (s & 0xffff) + (s >> 16);
    return htons(~s & 0xffff);
}

static void mac_str(const uint8_t *m, char *out) {
    sprintf(out, "%02x:%02x:%02x:%02x:%02x:%02x", m[0], m[1], m[2], m[3], m[4], m[5]);
}

// ---- vmnet start / io ----------------------------------------------------------------------

static int start(nic *n, vmnet_network_ref net, uint64_t mode, const char *bridge, int own_mac) {
    xpc_object_t desc = xpc_dictionary_create(NULL, NULL, 0);
    if (own_mac) xpc_dictionary_set_bool(desc, vmnet_allocate_mac_address_key, false);
    if (n->vhdr) xpc_dictionary_set_bool(desc, vmnet_enable_virtio_header_key, true);
    if (n->vhdr) xpc_dictionary_set_bool(desc, vmnet_enable_tso_key, true);
    dispatch_semaphore_t s = dispatch_semaphore_create(0);
    __block vmnet_return_t st = -1;
    char macbuf[32] = "", gwbuf[64] = "", maskbuf[64] = "";
    char *macs = macbuf, *gw = gwbuf, *mask = maskbuf;
    __block uint64_t maxp = 0, mtu = 0;
    n->q = dispatch_queue_create(n->name, 0);
    void (^done)(vmnet_return_t, xpc_object_t) = ^(vmnet_return_t r, xpc_object_t p) {
        st = r;
        if (p) {
            const char *m = xpc_dictionary_get_string(p, vmnet_mac_address_key);
            if (m) snprintf(macs, 32, "%s", m);
            maxp = xpc_dictionary_get_uint64(p, vmnet_max_packet_size_key);
            mtu = xpc_dictionary_get_uint64(p, vmnet_mtu_key);
            const char *g = xpc_dictionary_get_string(p, vmnet_start_address_key);
            const char *k = xpc_dictionary_get_string(p, vmnet_subnet_mask_key);
            if (g) snprintf(gw, 64, "%s", g);
            if (k) snprintf(mask, 64, "%s", k);
        }
        dispatch_semaphore_signal(s);
    };
    if (net) {
        n->ifc = vmnet_interface_start_with_network(net, desc, n->q, done);
    } else {
        xpc_dictionary_set_uint64(desc, vmnet_operation_mode_key, mode);
        if (bridge) xpc_dictionary_set_string(desc, vmnet_shared_interface_name_key, bridge);
        n->ifc = vmnet_start_interface(desc, n->q, done);
    }
    if (!n->ifc) { printf("[%s] start returned NULL\n", n->name); return -1; }
    dispatch_semaphore_wait(s, dispatch_time(DISPATCH_TIME_NOW, 10 * NSEC_PER_SEC));
    if (st != VMNET_SUCCESS) { printf("[%s] start status=%d\n", n->name, st); return -1; }
    if (!own_mac && macs[0]) {
        unsigned m[6];
        sscanf(macs, "%x:%x:%x:%x:%x:%x", &m[0], &m[1], &m[2], &m[3], &m[4], &m[5]);
        for (int i = 0; i < 6; i++) n->mac[i] = m[i];
    }
    n->max_pkt = maxp;
    char ms[20];
    mac_str(n->mac, ms);
    printf("[%s] started: mac=%s (%s) mtu=%llu max_packet=%llu start=%s mask=%s%s\n", n->name, ms,
           own_mac ? "ours" : "vmnet's", mtu, maxp, gw[0] ? gw : "-", mask[0] ? mask : "-",
           n->vhdr ? " virtio-hdr+tso" : "");
    return 0;
}

static void stop(nic *n) {
    if (!n->ifc) return;
    dispatch_semaphore_t s = dispatch_semaphore_create(0);
    vmnet_stop_interface(n->ifc, n->q, ^(vmnet_return_t r) { dispatch_semaphore_signal(s); });
    dispatch_semaphore_wait(s, dispatch_time(DISPATCH_TIME_NOW, 5 * NSEC_PER_SEC));
    n->ifc = NULL;
}

// Send one Ethernet frame (no virtio header; prepended here when the interface wants one).
static int tx(nic *n, const uint8_t *frame, size_t len) {
    uint8_t b[BUF];
    size_t off = n->vhdr ? 12 : 0;
    memset(b, 0, off);
    memcpy(b + off, frame, len);
    struct iovec iov = {b, len + off};
    struct vmpktdesc pd = {.vm_pkt_size = len + off, .vm_pkt_iov = &iov, .vm_pkt_iovcnt = 1};
    int count = 1;
    vmnet_return_t r = vmnet_write(n->ifc, &pd, &count);
    if (r != VMNET_SUCCESS || count != 1) {
        printf("[%s] vmnet_write status=%d count=%d\n", n->name, r, count);
        return -1;
    }
    return 0;
}

// Read one frame into f (header stripped); returns its length, 0 if none is pending.
static size_t rx(nic *n, uint8_t *f) {
    uint8_t b[BUF];
    struct iovec iov = {b, sizeof b};
    struct vmpktdesc pd = {.vm_pkt_size = sizeof b, .vm_pkt_iov = &iov, .vm_pkt_iovcnt = 1};
    int count = 1;
    if (vmnet_read(n->ifc, &pd, &count) != VMNET_SUCCESS || count != 1) return 0;
    size_t off = n->vhdr ? 12 : 0;
    if (pd.vm_pkt_size < off + 14) return 0;
    if (off)
        for (size_t i = 0; i < off; i++)
            if (b[i]) n->seen_vhdr_nonzero = 1;
    size_t len = pd.vm_pkt_size - off;
    n->rx_frames++;
    if (len > 1514) n->rx_big++;
    memcpy(f, b + off, len);
    return len;
}

// Reply to an ARP request for our own address, as any guest would.
static void answer_arp(nic *n, const uint8_t *f, size_t len) {
    if (len < 42 || f[12] != 0x08 || f[13] != 0x06 || f[21] != 1 || !n->ip.s_addr) return;
    if (memcmp(f + 38, &n->ip, 4)) return;
    uint8_t r[64] = {0};
    memcpy(r, f + 6, 6), memcpy(r + 6, n->mac, 6);
    r[12] = 0x08, r[13] = 0x06;
    uint8_t *a = r + 14;
    a[1] = 1, a[2] = 8, a[4] = 6, a[5] = 4, a[7] = 2;
    memcpy(a + 8, n->mac, 6), memcpy(a + 14, &n->ip, 4);
    memcpy(a + 18, f + 22, 6), memcpy(a + 24, f + 28, 4);
    tx(n, r, 42);
}

// Wait up to secs for a frame that match() accepts; returns its length or 0.
static size_t wait_for(nic *n, uint8_t *f, double secs, int (*match)(nic *, const uint8_t *, size_t)) {
    double end = now() + secs;
    while (now() < end) {
        size_t len = rx(n, f);
        if (!len) { usleep(2000); continue; }
        answer_arp(n, f, len);
        if (getenv("MINIGUEST_TRACE_WAIT") && !match(n, f, len)) {
            char s[20];
            mac_str(f + 6, s);
            printf("[%s] (unmatched) %zu bytes from %s type %02x%02x", n->name, len, s, f[12], f[13]);
            if (f[12] == 0x08 && f[13] == 0) {
                char a[20];
                strcpy(a, inet_ntoa(*(struct in_addr *)(f + 26)));
                printf(" proto %d %s -> %s", f[23], a, inet_ntoa(*(struct in_addr *)(f + 30)));
            }
            if (f[12] == 0x08 && f[13] == 6) printf(" arp op %d", f[21]);
            printf("\n");
        }
        if (match(n, f, len)) return len;
    }
    return 0;
}

// ---- frame builders --------------------------------------------------------------------------

static size_t eth(uint8_t *f, const uint8_t *dst, const uint8_t *src, uint16_t type) {
    memcpy(f, dst, 6);
    memcpy(f + 6, src, 6);
    f[12] = type >> 8, f[13] = type & 0xff;
    return 14;
}

static size_t ipv4(uint8_t *p, struct in_addr src, struct in_addr dst, uint8_t proto, size_t payload) {
    static uint16_t id = 1;
    p[0] = 0x45, p[1] = 0;
    uint16_t tot = htons(20 + payload);
    memcpy(p + 2, &tot, 2);
    uint16_t i = htons(id++);
    memcpy(p + 4, &i, 2);
    p[6] = 0x40, p[7] = 0, p[8] = 64, p[9] = proto, p[10] = p[11] = 0;
    memcpy(p + 12, &src, 4);
    memcpy(p + 16, &dst, 4);
    uint16_t c = csum(p, 20, 0);
    memcpy(p + 10, &c, 2);
    return 20;
}

static const uint8_t BCAST[6] = {0xff, 0xff, 0xff, 0xff, 0xff, 0xff};
static uint32_t xid = 0x4c494d31;  // "LIM1"

static void dhcp_send(nic *n, int type) {
    uint8_t f[600] = {0}, *p = f + 14, *u = p + 20, *d = u + 8;
    eth(f, BCAST, n->mac, 0x0800);
    d[0] = 1, d[1] = 1, d[2] = 6;
    uint32_t x = htonl(xid);
    memcpy(d + 4, &x, 4);
    d[10] = 0x80;  // broadcast flag: we have no address to receive a unicast reply on
    memcpy(d + 28, n->mac, 6);
    uint8_t *o = d + 236;
    o[0] = 99, o[1] = 130, o[2] = 83, o[3] = 99;
    o += 4;
    *o++ = 53, *o++ = 1, *o++ = type;
    *o++ = 55, *o++ = 4, *o++ = 1, *o++ = 3, *o++ = 6, *o++ = 26;
    if (type == 3) {
        *o++ = 50, *o++ = 4, memcpy(o, &n->ip, 4), o += 4;
        *o++ = 54, *o++ = 4, memcpy(o, &n->server, 4), o += 4;
    }
    *o++ = 255;
    size_t dlen = (o - d) < 300 ? 300 : (o - d);
    uint16_t sp = htons(68), dp = htons(67), ul = htons(8 + dlen);
    memcpy(u, &sp, 2), memcpy(u + 2, &dp, 2), memcpy(u + 4, &ul, 2);  // UDP checksum 0 = none
    struct in_addr any = {0}, all = {.s_addr = 0xffffffff};
    ipv4(p, any, all, 17, 8 + dlen);
    tx(n, f, 14 + 20 + 8 + dlen);
}

static int dhcp_type;
static int is_dhcp(nic *n, const uint8_t *f, size_t len) {
    if (len < 14 + 20 + 8 + 240 || f[12] != 0x08 || f[13] != 0 || f[14 + 9] != 17) return 0;
    const uint8_t *u = f + 14 + (f[14] & 0xf) * 4, *d = u + 8;
    if (u[2] != 0 || u[3] != 68) return 0;
    uint32_t x;
    memcpy(&x, d + 4, 4);
    if (ntohl(x) != xid || memcmp(d + 28, n->mac, 6)) return 0;
    memcpy(&n->ip, d + 16, 4);
    const uint8_t *o = d + 240, *end = f + len;
    dhcp_type = 0;
    while (o < end && *o != 255) {
        if (*o == 0) { o++; continue; }
        uint8_t t = o[0], l = o[1];
        const uint8_t *v = o + 2;
        if (t == 53) dhcp_type = v[0];
        if (t == 1) memcpy(&n->mask, v, 4);
        if (t == 3) memcpy(&n->router, v, 4);
        if (t == 6) memcpy(&n->dns, v, 4);
        if (t == 54) memcpy(&n->server, v, 4);
        if (t == 26) n->mtu = (v[0] << 8) | v[1];
        o += 2 + l;
    }
    return 1;
}

static int dhcp(nic *n) {
    uint8_t f[BUF];
    double t0 = now();
    for (int attempt = 0; attempt < 3; attempt++) {
        dhcp_send(n, 1);
        if (wait_for(n, f, 4, is_dhcp) && dhcp_type == 2) break;
        if (attempt == 2) { printf("[%s] DHCP: no OFFER\n", n->name); return -1; }
    }
    dhcp_send(n, 3);
    if (!wait_for(n, f, 4, is_dhcp) || dhcp_type != 5) {
        printf("[%s] DHCP: no ACK (last type %d)\n", n->name, dhcp_type);
        return -1;
    }
    char a[4][20];
    strcpy(a[0], inet_ntoa(n->ip)), strcpy(a[1], inet_ntoa(n->mask));
    strcpy(a[2], inet_ntoa(n->router)), strcpy(a[3], inet_ntoa(n->dns));
    printf("[%s] DHCP lease in %.2f s: ip=%s mask=%s router=%s dns=%s server=%s mtu=%u\n", n->name,
           now() - t0, a[0], a[1], a[2], a[3], inet_ntoa(n->server), n->mtu);
    return 0;
}

static struct in_addr arp_target;
static int is_arp_reply(nic *n, const uint8_t *f, size_t len) {
    if (len < 42 || f[12] != 0x08 || f[13] != 0x06 || f[21] != 2) return 0;
    if (memcmp(f + 28, &arp_target, 4)) return 0;
    memcpy(n->router_mac, f + 22, 6);
    return 1;
}

// ARP for target; the answering MAC lands in n->router_mac. Also answers ARP requests for our
// own address while waiting, so a peer (or the host) can find us.
static int arp(nic *n, struct in_addr target, double secs) {
    uint8_t f[64] = {0};
    eth(f, BCAST, n->mac, 0x0806);
    uint8_t *a = f + 14;
    a[1] = 1, a[2] = 8, a[4] = 6, a[5] = 4, a[7] = 1;
    memcpy(a + 8, n->mac, 6), memcpy(a + 14, &n->ip, 4), memcpy(a + 24, &target, 4);
    arp_target = target;
    tx(n, f, 42);
    uint8_t r[BUF];
    return wait_for(n, r, secs, is_arp_reply) ? 0 : -1;
}

// Answer ARP requests for our address and count TCP SYNs to `port`, for secs.
static int serve(nic *n, double secs, int port, int *syns) {
    uint8_t f[BUF];
    double end = now() + secs;
    while (now() < end) {
        size_t len = rx(n, f);
        if (!len) { usleep(2000); continue; }
        if (getenv("MINIGUEST_TRACE")) {
            char s[20], d[20];
            mac_str(f + 6, s);
            mac_str(f, d);
            printf("[%s] rx %zu bytes %s -> %s type %02x%02x", n->name, len, s, d, f[12], f[13]);
            if (f[12] == 0x08 && f[13] == 0x06) {
                char sp[20];
                strcpy(sp, inet_ntoa(*(struct in_addr *)(f + 28)));
                printf(" arp op %d %s asks %s", f[21], sp, inet_ntoa(*(struct in_addr *)(f + 38)));
            }
            if (f[12] == 0x08 && f[13] == 0) {
                char sp[20];
                strcpy(sp, inet_ntoa(*(struct in_addr *)(f + 26)));
                printf(" ip proto %d %s -> %s", f[23], sp, inet_ntoa(*(struct in_addr *)(f + 30)));
            }
            printf("\n");
        }
        if (f[12] == 0x08 && f[13] == 0x06 && f[21] == 1 && !memcmp(f + 38, &n->ip, 4)) {
            uint8_t r[64] = {0};
            eth(r, f + 6, n->mac, 0x0806);
            uint8_t *a = r + 14;
            a[1] = 1, a[2] = 8, a[4] = 6, a[5] = 4, a[7] = 2;
            memcpy(a + 8, n->mac, 6), memcpy(a + 14, &n->ip, 4);
            memcpy(a + 18, f + 22, 6), memcpy(a + 24, f + 28, 4);
            tx(n, r, 42);
        }
        // answer ICMP echo requests to our address, so a peer can ping us through a router
        if (f[12] == 0x08 && f[13] == 0 && f[14 + 9] == 1 && !memcmp(f + 30, &n->ip, 4) &&
            f[14 + (f[14] & 0xf) * 4] == 8 && len < 1500) {
            uint8_t r[1514];
            memcpy(r, f, len);
            memcpy(r, f + 6, 6), memcpy(r + 6, n->mac, 6);
            memcpy(r + 26, f + 30, 4), memcpy(r + 30, f + 26, 4);
            r[24] = r[25] = 0;
            uint16_t c = csum(r + 14, 20, 0);
            memcpy(r + 24, &c, 2);
            uint8_t *ic = r + 14 + (f[14] & 0xf) * 4;
            size_t ilen = len - (ic - r);
            ic[0] = 0, ic[2] = ic[3] = 0;
            c = csum(ic, ilen, 0);
            memcpy(ic + 2, &c, 2);
            tx(n, r, len);
            char src[20];
            strcpy(src, inet_ntoa(*(struct in_addr *)(f + 26)));
            printf("[%s] answered ping from %s\n", n->name, src);
        }
        if (syns && f[12] == 0x08 && f[13] == 0 && f[14 + 9] == 6) {
            const uint8_t *t = f + 14 + (f[14] & 0xf) * 4;
            if (((t[2] << 8) | t[3]) == port && (t[13] & 0x02)) {
                char src[20];
                strcpy(src, inet_ntoa(*(struct in_addr *)(f + 26)));
                printf("[%s] SYN to :%d from %s:%d\n", n->name, port, src, (t[0] << 8) | t[1]);
                (*syns)++;
            }
        }
    }
    return 0;
}

static struct in_addr ping_dst;
static int dns_port;
static int is_dns_reply(nic *n, const uint8_t *f, size_t len) {
    if (len < 14 + 20 + 8 + 12 || f[12] != 0x08 || f[13] != 0 || f[14 + 9] != 17) return 0;
    const uint8_t *u = f + 14 + (f[14] & 0xf) * 4;
    return ((u[0] << 8) | u[1]) == 53 && ((u[2] << 8) | u[3]) == dns_port;
}

// A UDP DNS query for example.com (A) to dst:53 via the router; returns the RTT in ms, or -1.
static double dns(nic *n, struct in_addr dst) {
    static const uint8_t q[] = {0x4c, 0x4d, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 7, 'e', 'x', 'a', 'm', 'p',
                                'l', 'e', 3, 'c', 'o', 'm', 0, 0, 1, 0, 1};
    uint8_t f[128] = {0}, *p = f + 14, *u = p + 20;
    eth(f, n->router_mac, n->mac, 0x0800);
    dns_port = 40000 + (rand() % 20000);
    uint16_t sp = htons(dns_port), dp = htons(53), ul = htons(8 + sizeof q);
    memcpy(u, &sp, 2), memcpy(u + 2, &dp, 2), memcpy(u + 4, &ul, 2);
    memcpy(u + 8, q, sizeof q);
    ipv4(p, n->ip, dst, 17, 8 + sizeof q);
    double t0 = now();
    tx(n, f, 14 + 20 + 8 + sizeof q);
    uint8_t r[BUF];
    return wait_for(n, r, 3, is_dns_reply) ? (now() - t0) * 1000 : -1;
}

static int is_echo_reply(nic *n, const uint8_t *f, size_t len) {
    return len >= 42 && f[12] == 0x08 && f[13] == 0 && f[14 + 9] == 1 &&
           !memcmp(f + 26, &ping_dst, 4) && f[14 + (f[14] & 0xf) * 4] == 0;
}

// ICMP echo to dst via the router's MAC; returns the RTT in ms, or -1.
static double ping(nic *n, struct in_addr dst) {
    uint8_t f[128] = {0}, *p = f + 14, *ic = p + 20;
    eth(f, n->router_mac, n->mac, 0x0800);
    static uint16_t seq = 0;
    seq++;
    ic[0] = 8, ic[4] = 0x12, ic[5] = 0x34, ic[6] = seq >> 8, ic[7] = seq & 0xff;
    memcpy(ic + 8, "limina-vmnet-probe", 18);
    uint16_t c = csum(ic, 26, 0);
    memcpy(ic + 2, &c, 2);
    ipv4(p, n->ip, dst, 1, 26);
    ping_dst = dst;
    double t0 = now();
    tx(n, f, 14 + 20 + 26);
    uint8_t r[BUF];
    return wait_for(n, r, 3, is_echo_reply) ? (now() - t0) * 1000 : -1;
}

// ---- modes ------------------------------------------------------------------------------------

static int lease(const char *mode_s, const char *ifname, int vhdr, int hold) {
    uint64_t mode = !strcmp(mode_s, "shared") ? VMNET_SHARED_MODE
                    : !strcmp(mode_s, "host") ? VMNET_HOST_MODE
                                              : VMNET_BRIDGED_MODE;
    if (mode == VMNET_BRIDGED_MODE) {
        xpc_object_t ifs = vmnet_copy_shared_interface_list();
        printf("bridgeable interfaces:");
        for (size_t i = 0; ifs && i < xpc_array_get_count(ifs); i++)
            printf(" %s", xpc_array_get_string(ifs, i));
        printf("\n");
        if (!ifname) ifname = ifs && xpc_array_get_count(ifs) ? xpc_array_get_string(ifs, 0) : NULL;
    }
    nic n = {.name = mode_s, .vhdr = vhdr};
    if (start(&n, NULL, mode, mode == VMNET_BRIDGED_MODE ? ifname : NULL, 0)) return 1;
    if (mode == VMNET_BRIDGED_MODE) printf("[%s] bridged on %s\n", n.name, ifname);
    int rc = 1;
    if (dhcp(&n)) goto out;
    if (!n.router.s_addr) n.router = n.server;  // host mode: no router; talk to the host instead
    if (arp(&n, n.router, 3)) { printf("[%s] router %s: no ARP reply\n", n.name, inet_ntoa(n.router)); goto out; }
    char rm[20];
    mac_str(n.router_mac, rm);
    printf("[%s] router %s is at %s\n", n.name, inet_ntoa(n.router), rm);
    double r1 = ping(&n, n.router);
    printf("[%s] ping router: %s", n.name, r1 < 0 ? "no reply\n" : "");
    if (r1 >= 0) printf("%.2f ms\n", r1);
    rc = r1 < 0;
    if (mode != VMNET_HOST_MODE) {
        struct in_addr ext;
        inet_aton("1.1.1.1", &ext);
        double r2 = ping(&n, ext);
        printf("[%s] ping 1.1.1.1 through the router: %s", n.name, r2 < 0 ? "no reply\n" : "");
        if (r2 >= 0) printf("%.2f ms\n", r2);
        rc |= r2 < 0;
    }
    // Hold the interface and ping 1.1.1.1 once a second, so a VPN can be toggled under a live guest.
    for (int i = 0; i < hold; i++) {
        struct in_addr ext;
        inet_aton("1.1.1.1", &ext);
        double r = ping(&n, ext), d = dns(&n, ext);
        time_t t = time(NULL);
        char ts[16];
        strftime(ts, sizeof ts, "%H:%M:%S", localtime(&t));
        printf("[%s] %s hold 1.1.1.1: icmp %s, udp dns %s\n", n.name, ts, r < 0 ? "NO REPLY" : "reply",
               d < 0 ? "NO REPLY" : "reply");
        if (r >= 0 && d >= 0) usleep(1000000);
    }
out:
    if (vhdr)
        printf("[%s] virtio-hdr: %d frames read, %d over 1514 bytes, non-zero header seen: %s\n",
               n.name, n.rx_frames, n.rx_big, n.seen_vhdr_nonzero ? "yes" : "no");
    stop(&n);
    printf("RESULT %s%s: %s\n", mode_s, vhdr ? " vhdr" : "", rc ? "FAIL" : "PASS");
    return rc;
}

// custom bits: 1 = pin the subnet (MINIGUEST_SUBNET, default 192.168.211.1), 2 = DHCP reservation
// for reserve_mac, 4 = port forward host:fwd_port -> reserved address:22. NULL reserve_mac = 0.
static vmnet_network_ref make_network_bits(int bits, const uint8_t *reserve_mac,
                                           struct in_addr *reserve_ip, uint16_t fwd_port);
static vmnet_network_ref make_network(const uint8_t *reserve_mac, struct in_addr *reserve_ip,
                                      uint16_t fwd_port) {
    return make_network_bits(reserve_mac ? 7 : 0, reserve_mac, reserve_ip, fwd_port);
}

static vmnet_network_ref make_network_bits(int bits, const uint8_t *reserve_mac,
                                           struct in_addr *reserve_ip, uint16_t fwd_port) {
    vmnet_return_t st = 0;
    vmnet_network_configuration_ref cfg = vmnet_network_configuration_create(VMNET_SHARED_MODE, &st);
    if (!cfg) { printf("configuration_create status=%d\n", st); return NULL; }
    const char *subnet = getenv("MINIGUEST_SUBNET") ? getenv("MINIGUEST_SUBNET") : "192.168.211.1";
    if (bits & 1) {
        struct in_addr sub, mask;
        inet_aton(subnet, &sub), inet_aton("255.255.255.0", &mask);
        st = vmnet_network_configuration_set_ipv4_subnet(cfg, &sub, &mask);
        printf("set_ipv4_subnet %s/24: %d\n", subnet, st);
    }
    if (reserve_ip) {
        // The reservation sits in the pinned subnet (or the default 192.168.65 one).
        struct in_addr base;
        inet_aton(bits & 1 ? subnet : "192.168.65.1", &base);
        reserve_ip->s_addr = htonl((ntohl(base.s_addr) & 0xffffff00) | 77);
    }
    if (bits & 2) {
        st = vmnet_network_configuration_add_dhcp_reservation(cfg, (const ether_addr_t *)reserve_mac,
                                                              reserve_ip);
        printf("add_dhcp_reservation -> %s: %d\n", inet_ntoa(*reserve_ip), st);
    }
    if (bits & 4) {
        st = vmnet_network_configuration_add_port_forwarding_rule(cfg, IPPROTO_TCP, AF_INET, 22,
                                                                  fwd_port, reserve_ip);
        printf("add_port_forwarding_rule host:%u -> %s:22: %d\n", fwd_port, inet_ntoa(*reserve_ip), st);
    }
    vmnet_network_ref net = vmnet_network_create(cfg, &st);
    if (!net) { printf("network_create status=%d\n", st); return NULL; }
    struct in_addr s, m;
    vmnet_network_get_ipv4_subnet(net, &s, &m);
    char a[20];
    strcpy(a, inet_ntoa(s));
    printf("network subnet %s mask %s\n", a, inet_ntoa(m));
    return net;
}

// One interface on a network-object network: which setting breaks the start? custom = pinned
// subnet + reservation + port forward; own_mac = vmnet_allocate_mac_address_key false.
static int netobj_one(int custom, int own_mac) {
    nic a = {.name = "one", .mac = {0x02, 0x4c, 0x4d, 0x00, 0x00, 0x0a}};
    struct in_addr reserved;
    vmnet_network_ref net = make_network_bits(custom, a.mac, &reserved, 2299);
    if (!net || start(&a, net, 0, NULL, own_mac)) return 1;
    int rc = dhcp(&a) || arp(&a, a.router, 3);
    if (custom & 2) printf("[one] reservation honoured: %s\n", a.ip.s_addr == reserved.s_addr ? "yes" : "NO");
    if (!rc) {
        struct in_addr ext;
        inet_aton("1.1.1.1", &ext);
        double r = ping(&a, ext);
        printf("[one] ping 1.1.1.1: %.2f ms\n", r);
        rc = r < 0;
    }
    stop(&a);
    printf("RESULT netobj-one custom=%d own_mac=%d: %s\n", custom, own_mac, rc ? "FAIL" : "PASS");
    return rc;
}

static int netobj(void) {
    const uint16_t fwd = 2299;
    nic a = {.name = "net1-a", .mac = {0x02, 0x4c, 0x4d, 0x00, 0x00, 0x0a}};
    nic b = {.name = "net1-b", .mac = {0x02, 0x4c, 0x4d, 0x00, 0x00, 0x0b}};
    nic c = {.name = "net2-c", .mac = {0x02, 0x4c, 0x4d, 0x00, 0x00, 0x0c}};
    struct in_addr reserved;
    int ok = 1;
    vmnet_network_ref n1 = make_network(a.mac, &reserved, fwd);
    vmnet_network_ref n2 = make_network(NULL, NULL, 0);
    if (!n1 || !n2) return 1;
    if (start(&a, n1, 0, NULL, 1) || start(&b, n1, 0, NULL, 1) || start(&c, n2, 0, NULL, 1)) return 1;
    if (dhcp(&a) || dhcp(&b) || dhcp(&c)) return 1;
    printf("reservation honoured: %s\n", a.ip.s_addr == reserved.s_addr ? "yes" : "NO");
    ok &= a.ip.s_addr == reserved.s_addr;

    // a and b on one network must reach each other; c on the other must not reach a.
    // Each answers ARP for itself on its own queue while the other asks.
    nic *pa = &a, *pb = &b;
    dispatch_group_t g = dispatch_group_create();
    dispatch_group_async(g, dispatch_get_global_queue(0, 0), ^{ serve(pb, 3, 0, NULL); });
    int ab = arp(&a, b.ip, 3) == 0;
    dispatch_group_wait(g, DISPATCH_TIME_FOREVER);
    dispatch_group_async(g, dispatch_get_global_queue(0, 0), ^{ serve(pa, 3, 0, NULL); });
    int ca = arp(&c, a.ip, 3) == 0;
    dispatch_group_wait(g, DISPATCH_TIME_FOREVER);
    printf("same network  a -> b ARP: %s\n", ab ? "reached" : "NOT reached");
    printf("other network c -> a ARP: %s\n", ca ? "REACHED (not isolated)" : "not reached (isolated)");
    ok &= ab && !ca;

    // L3: c pings a through its own router; the host routes both bridge subnets.
    if (arp(&c, c.router, 3) == 0) {
        dispatch_group_async(g, dispatch_get_global_queue(0, 0), ^{ serve(pa, 4, 0, NULL); });
        double r = ping(&c, a.ip);
        dispatch_group_wait(g, DISPATCH_TIME_FOREVER);
        printf("other network c -> a ping via router: %s\n", r < 0 ? "no reply (isolated)" : "REPLY (routed between networks)");
        ok &= r < 0;
    }

    // Internet through the network's NAT, from a.
    if (arp(&a, a.router, 3) == 0) {
        struct in_addr ext;
        inet_aton("1.1.1.1", &ext);
        double r = ping(&a, ext);
        printf("net1-a ping 1.1.1.1: %s", r < 0 ? "no reply\n" : "");
        if (r >= 0) printf("%.2f ms\n", r);
        ok &= r >= 0;
    } else {
        printf("net1-a: router ARP failed\n");
        ok = 0;
    }

    // Port forward: a host connect() to the forwarded port must show up as a SYN on a.
    __block int syns = 0;
    dispatch_group_async(g, dispatch_get_global_queue(0, 0), ^{ serve(pa, 12, 22, &syns); });
    usleep(200000);
    // Try loopback, the host side of the vmnet bridge, and every other IPv4 address the host has.
    struct ifaddrs *ifs, *i;
    getifaddrs(&ifs);
    for (i = ifs; i; i = i->ifa_next) {
        if (!i->ifa_addr || i->ifa_addr->sa_family != AF_INET) continue;
        struct sockaddr_in sa = *(struct sockaddr_in *)i->ifa_addr;
        sa.sin_port = htons(fwd);
        int before = syns;
        int s = socket(AF_INET, SOCK_STREAM, 0);
        fcntl(s, F_SETFL, O_NONBLOCK);
        int r = connect(s, (struct sockaddr *)&sa, sizeof sa);
        char addr[20];
        strcpy(addr, inet_ntoa(sa.sin_addr));
        usleep(500000);
        printf("host connect %s (%s):%u -> %s; SYNs now %d (was %d)\n", addr, i->ifa_name, fwd,
               r == 0 ? "connected" : errno == EINPROGRESS ? "in progress" : strerror(errno), syns, before);
        close(s);
    }
    freeifaddrs(ifs);
    // The host reaching the guest directly, no forward: a connect() to its address on the bridge.
    {
        int before = syns;
        struct sockaddr_in sa = {.sin_family = AF_INET, .sin_port = htons(22), .sin_addr = a.ip};
        int s = socket(AF_INET, SOCK_STREAM, 0);
        fcntl(s, F_SETFL, O_NONBLOCK);
        connect(s, (struct sockaddr *)&sa, sizeof sa);
        usleep(500000);
        printf("host connect %s:22 directly -> SYNs now %d (was %d)\n", inet_ntoa(a.ip), syns, before);
        close(s);
    }
    dispatch_group_wait(g, DISPATCH_TIME_FOREVER);
    printf("port forward SYNs seen on net1-a: %d\n", syns);
    ok &= syns > 0;

    stop(&a), stop(&b), stop(&c);
    printf("RESULT netobj: %s\n", ok ? "PASS" : "FAIL");
    return !ok;
}

// ---- uplink: what the host routes 1.1.1.1 through, and what SystemConfiguration calls primary ----

// The interface of the host's route to 1.1.1.1, via an RTM_GET on the routing socket.
static int route_ifname(char *out) {
    out[0] = 0;
    int s = socket(PF_ROUTE, SOCK_RAW, 0);
    if (s < 0) return -1;
    struct {
        struct rt_msghdr h;
        struct sockaddr_in dst;
        char pad[512];
    } m;
    memset(&m, 0, sizeof m);
    static int seq;
    m.h.rtm_msglen = sizeof m.h + sizeof m.dst;
    m.h.rtm_version = RTM_VERSION;
    m.h.rtm_type = RTM_GET;
    m.h.rtm_addrs = RTA_DST;
    m.h.rtm_pid = getpid();
    m.h.rtm_seq = ++seq;
    m.dst.sin_len = sizeof m.dst;
    m.dst.sin_family = AF_INET;
    inet_aton("1.1.1.1", &m.dst.sin_addr);
    if (write(s, &m, m.h.rtm_msglen) < 0) { close(s); return -1; }
    for (;;) {
        ssize_t r = read(s, &m, sizeof m);
        if (r < (ssize_t)sizeof m.h) { close(s); return -1; }
        if (m.h.rtm_pid == getpid() && m.h.rtm_seq == seq) break;
    }
    close(s);
    return if_indextoname(m.h.rtm_index, out) ? 0 : -1;
}

// State:/Network/Global/IPv4 PrimaryInterface.
static void sc_primary(char *out, size_t len) {
    static SCDynamicStoreRef store;
    out[0] = 0;
    if (!store) store = SCDynamicStoreCreate(NULL, CFSTR("miniguest"), NULL, NULL);
    CFDictionaryRef d = store ? SCDynamicStoreCopyValue(store, CFSTR("State:/Network/Global/IPv4")) : NULL;
    if (!d) return;
    CFStringRef p = CFDictionaryGetValue(d, CFSTR("PrimaryInterface"));
    if (p) CFStringGetCString(p, out, len, kCFStringEncodingUTF8);
    CFRelease(d);
}

// A shared network NATing out of ext (NULL = vmnet's default), with the time the create took.
static vmnet_network_ref make_ext_network(const char *ext) {
    vmnet_return_t st = 0;
    vmnet_network_configuration_ref cfg = vmnet_network_configuration_create(VMNET_SHARED_MODE, &st);
    if (!cfg) { printf("configuration_create status=%d\n", st); return NULL; }
    if (ext) {
        st = vmnet_network_configuration_set_external_interface(cfg, ext);
        printf("set_external_interface %s: %d\n", ext, st);
    }
    double t0 = now();
    vmnet_network_ref net = vmnet_network_create(cfg, &st);
    CFRelease(cfg);
    if (!net) { printf("network_create status=%d\n", st); return NULL; }
    struct in_addr s, m;
    vmnet_network_get_ipv4_subnet(net, &s, &m);
    printf("network created in %.2f s, gateway %s\n", now() - t0, inet_ntoa(s));
    return net;
}

static int bring_up(nic *n, vmnet_network_ref net) {
    n->ip.s_addr = n->router.s_addr = 0;
    if (start(n, net, 0, NULL, 1) || dhcp(n) || arp(n, n->router, 3)) return -1;
    return 0;
}

// ext <ifname|default|follow> --hold N: a shared network on a chosen uplink, checked once a second.
// follow picks the uplink the host routes 1.1.1.1 through, polls it every check, and on a change
// tears the interface and network down and rebuilds them on the new uplink with the same guest MAC.
static int ext_mode(const char *which, int hold) {
    int follow = !strcmp(which, "follow");
    char up[IFNAMSIZ] = "", cur[IFNAMSIZ] = "", sc[64] = "";
    route_ifname(up);
    sc_primary(sc, sizeof sc);
    printf("host: route to 1.1.1.1 via %s; SystemConfiguration primary %s\n", up, sc);
    const char *ext = follow ? up : !strcmp(which, "default") ? NULL : which;
    nic n = {.name = "ext", .mac = {0x02, 0x4c, 0x4d, 0x00, 0x00, 0x0e}};
    vmnet_network_ref net = make_ext_network(ext);
    if (!net || bring_up(&n, net)) { printf("RESULT ext %s: FAIL (bring-up)\n", which); return 1; }
    int ok = 0, bad = 0;
    for (int i = 0; i < hold; i++) {
        if (follow && route_ifname(cur) == 0 && cur[0] && strcmp(cur, up)) {
            sc_primary(sc, sizeof sc);
            double t0 = now();
            printf("[ext] uplink %s -> %s (SystemConfiguration primary %s); rebuilding\n", up, cur, sc);
            strcpy(up, cur);
            stop(&n);
            CFRelease(net);
            net = make_ext_network(up);
            if (!net || bring_up(&n, net)) { printf("[ext] rebuild FAILED\n"); break; }
            printf("[ext] rebuilt on %s in %.2f s, guest %s\n", up, now() - t0, inet_ntoa(n.ip));
        }
        struct in_addr ext1;
        inet_aton("1.1.1.1", &ext1);
        double r = ping(&n, ext1), d = dns(&n, ext1);
        time_t t = time(NULL);
        char ts[16];
        strftime(ts, sizeof ts, "%H:%M:%S", localtime(&t));
        route_ifname(cur);
        printf("[ext] %s host route %s: icmp %s, udp dns %s\n", ts, cur, r < 0 ? "NO REPLY" : "reply",
               d < 0 ? "NO REPLY" : "reply");
        if (r >= 0 && d >= 0) ok++, usleep(1000000);
        else bad++;
    }
    stop(&n);
    if (net) CFRelease(net);
    printf("RESULT ext %s: %d answered, %d failed\n", which, ok, bad);
    return bad > 0;
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    printf("uid=%d\n", getuid());
    if (argc >= 3 && !strcmp(argv[1], "lease")) {
        int vhdr = 0, hold = 0;
        const char *ifname = NULL;
        for (int i = 3; i < argc; i++) {
            if (!strcmp(argv[i], "--vhdr")) vhdr = 1;
            else if (!strcmp(argv[i], "--hold") && i + 1 < argc) hold = atoi(argv[++i]);
            else ifname = argv[i];
        }
        return lease(argv[2], ifname, vhdr, hold);
    }
    if (argc == 2 && !strcmp(argv[1], "netobj")) return netobj();
    if (argc == 5 && !strcmp(argv[1], "ext") && !strcmp(argv[3], "--hold")) return ext_mode(argv[2], atoi(argv[4]));
    if (argc == 4 && !strcmp(argv[1], "netobj-one")) return netobj_one(atoi(argv[2]), atoi(argv[3]));
    fprintf(stderr, "usage: %s lease <shared|host|bridged> [ifname] [--vhdr] [--hold secs] | netobj | ext <ifname|default|follow> --hold secs\n", argv[0]);
    return 2;
}
