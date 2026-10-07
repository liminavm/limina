// Can a non-root process use vmnet, and with which entitlements?
//
// For each mode it starts an interface and then proves packets flow rather than trusting the
// start status: shared and host-only ARP for the vmnet gateway and wait for the reply; bridged
// listens for any LAN frame for 5 s. Also tries the macOS 26 network-object API (shared mode,
// with a port-forward rule). Sign it with different entitlements and compare (see RESULTS.md).
#include <arpa/inet.h>
#include <dispatch/dispatch.h>
#include <netinet/in.h>
#include <stdio.h>
#include <string.h>
#include <sys/uio.h>
#include <unistd.h>
#include <vmnet/vmnet.h>

static const uint8_t our_mac[6] = {0x02, 0x11, 0x22, 0x33, 0x44, 0x55};

static size_t arp_request(uint8_t *f, struct in_addr sender, struct in_addr target) {
    memset(f, 0xff, 6);              // dst: broadcast
    memcpy(f + 6, our_mac, 6);       // src
    f[12] = 0x08, f[13] = 0x06;      // ethertype ARP
    uint8_t *a = f + 14;
    a[0] = 0, a[1] = 1, a[2] = 0x08, a[3] = 0, a[4] = 6, a[5] = 4, a[6] = 0, a[7] = 1;  // request
    memcpy(a + 8, our_mac, 6);
    memcpy(a + 14, &sender, 4);
    memset(a + 18, 0, 6);
    memcpy(a + 24, &target, 4);
    return 14 + 28;
}

static int read_frames(interface_ref ifc, int want_arp_reply, int secs) {
    uint8_t buf[2048];
    int frames = 0;
    for (int tries = 0; tries < secs * 20; tries++) {
        struct iovec iov = {buf, sizeof buf};
        struct vmpktdesc pd = {.vm_pkt_size = sizeof buf, .vm_pkt_iov = &iov, .vm_pkt_iovcnt = 1};
        int count = 1;
        if (vmnet_read(ifc, &pd, &count) == VMNET_SUCCESS && count == 1) {
            frames++;
            if (want_arp_reply && buf[12] == 0x08 && buf[13] == 0x06 && buf[21] == 2) return frames;
            if (!want_arp_reply) return frames;
        } else {
            usleep(50000);
        }
    }
    return want_arp_reply ? -frames : frames;
}

static void run(const char *what, vmnet_network_ref net, uint64_t mode, const char *bridge) {
    xpc_object_t desc = xpc_dictionary_create(NULL, NULL, 0);
    dispatch_semaphore_t s = dispatch_semaphore_create(0);
    __block vmnet_return_t st = -1;
    char gwbuf[64] = "", maskbuf[64] = "";
    char *gw = gwbuf, *mask = maskbuf;
    interface_ref ifc;
    dispatch_queue_t q = dispatch_queue_create("probe", 0);
    void (^done)(vmnet_return_t, xpc_object_t) = ^(vmnet_return_t r, xpc_object_t p) {
        st = r;
        if (p) {
            const char *g = xpc_dictionary_get_string(p, vmnet_start_address_key);
            const char *m = xpc_dictionary_get_string(p, vmnet_subnet_mask_key);
            if (g) snprintf(gw, 64, "%s", g);
            if (m) snprintf(mask, 64, "%s", m);
        }
        dispatch_semaphore_signal(s);
    };
    if (net) {
        ifc = vmnet_interface_start_with_network(net, desc, q, done);
    } else {
        xpc_dictionary_set_uint64(desc, vmnet_operation_mode_key, mode);
        if (bridge) xpc_dictionary_set_string(desc, vmnet_shared_interface_name_key, bridge);
        ifc = vmnet_start_interface(desc, q, done);
    }
    if (!ifc) { printf("%-24s start returned NULL\n", what); return; }
    dispatch_semaphore_wait(s, dispatch_time(DISPATCH_TIME_NOW, 10 * NSEC_PER_SEC));
    if (st != VMNET_SUCCESS) { printf("%-24s start status=%d (fail)\n", what, st); return; }
    if (bridge) {
        int n = read_frames(ifc, 0, 5);
        printf("%-24s start OK; LAN frames received in 5 s: %d\n", what, n);
    } else {
        // the start address is the vmnet gateway; use the next address as ours
        struct in_addr g, me;
        inet_aton(gw, &g);
        me.s_addr = htonl(ntohl(g.s_addr) + 1);
        uint8_t f[64];
        size_t len = arp_request(f, me, g);
        struct iovec iov = {f, len};
        struct vmpktdesc pd = {.vm_pkt_size = len, .vm_pkt_iov = &iov, .vm_pkt_iovcnt = 1};
        int count = 1;
        vmnet_return_t w = vmnet_write(ifc, &pd, &count);
        int n = read_frames(ifc, 1, 3);
        printf("%-24s start OK gw=%s/%s; ARP write=%d; %s\n", what, gw, mask, w,
               n > 0 ? "ARP reply from gateway RECEIVED" : "no ARP reply");
    }
    dispatch_semaphore_t s2 = dispatch_semaphore_create(0);
    vmnet_stop_interface(ifc, q, ^(vmnet_return_t r) { dispatch_semaphore_signal(s2); });
    dispatch_semaphore_wait(s2, dispatch_time(DISPATCH_TIME_NOW, 5 * NSEC_PER_SEC));
}

int main(void) {
    printf("uid=%d\n", getuid());
    vmnet_return_t st = 0;
    vmnet_network_configuration_ref cfg = vmnet_network_configuration_create(VMNET_SHARED_MODE, &st);
    if (cfg) {
        struct in_addr guest = {.s_addr = htonl(0xC0A84002)};
        vmnet_network_configuration_add_port_forwarding_rule(cfg, IPPROTO_TCP, AF_INET, 22, 2299, &guest);
        vmnet_network_ref net = vmnet_network_create(cfg, &st);
        if (net) run("network-object shared", net, 0, NULL);
        else printf("%-24s network_create failed status=%d\n", "network-object shared", st);
    }
    run("classic shared", NULL, VMNET_SHARED_MODE, NULL);
    run("classic host-only", NULL, VMNET_HOST_MODE, NULL);
    xpc_object_t ifs = vmnet_copy_shared_interface_list();
    const char *first = ifs && xpc_array_get_count(ifs) ? xpc_array_get_string(ifs, 0) : NULL;
    if (first) run("classic bridged", NULL, VMNET_BRIDGED_MODE, first);
    return 0;
}
