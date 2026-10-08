# vmnet-network-probe — what does a non-root process need to use vmnet?

`probe.c` starts a vmnet interface in each mode and proves packets flow instead of trusting the
start status: shared and host-only send an ARP request for the vmnet gateway and wait for the
reply; bridged listens 5 s for any LAN frame on the first bridgeable interface. It also tries the
macOS 26 network-object API (`vmnet_network_create`, shared mode, with a port-forward rule).

Build: `xcrun clang -mmacosx-version-min=26.0 -framework vmnet -o vmnet-probe probe.c`, then
sign ad-hoc with each entitlement set and run as the normal user (uid 501).

## Measured 2026-10-07, dev Mac (M1 Max, macOS 26.6.2), non-root

| Entitlements | network-object shared | classic shared | classic host-only | classic bridged (`en0`) |
|---|---|---|---|---|
| none | create fails (1002) | start fails (1001) | start fails (1001) | start fails (1001) |
| `com.apple.security.hypervisor` | create fails (1002) | start fails (1001) | start fails (1001) | start fails (1001) |
| `com.apple.security.virtualization` | starts | **ARP reply from gateway** | **ARP reply from gateway** | **LAN frame received** |
| both (`both.entitlements`) | starts | **ARP reply from gateway** | **ARP reply from gateway** | **LAN frame received** |

- `com.apple.security.virtualization` is unrestricted (ad-hoc signing is enough, like
  `com.apple.security.hypervisor`), and it is what unlocks vmnet for a non-root process — in all
  three modes, bridged included. The restricted `com.apple.vm.networking` is not involved.
- `com.apple.security.hypervisor` alone, which `limina-vmm` carries today, is not enough.
- The two entitlements coexist on one binary.
- The network-object interface started but returned no gateway address in its parameters, so
  the probe had nothing to ARP for; whether packets flow on that path is not established here.
- Not measured: macOS 27; the 16 KiB-page/throughput behaviour of vmnet under load; whether a
  Developer ID + notarized build behaves the same as ad-hoc.

# miniguest — what a VM attached to vmnet actually gets

`miniguest.c` plays a guest's network stack with raw frames over `vmnet_read`/`vmnet_write`: DHCP,
ARP, ICMP echo, an ARP/echo responder and a TCP SYN counter. `./run.sh lease <shared|host|bridged>
[ifname] [--vhdr]` or `./run.sh netobj` builds it, signs it ad-hoc with `both.entitlements` and runs
it non-root under `gtimeout`. `MINIGUEST_TRACE=1` prints every frame the responder sees.

## Measured 2026-10-08, dev Mac (M1 Max, macOS 26.6.2, Wi-Fi `en0`, Tailscale up, no exit node)

| test | result |
|---|---|
| classic shared | lease 192.168.65.x in 0.05 s, router/DNS = .1, ping router and 1.1.1.1 through the NAT answered (RTTs are quantised by a 2 ms poll, not latency figures) |
| classic host-only | lease 192.168.128.x in 0.06 s, **no router option**; the host (.1) answers ARP and ping |
| classic bridged on `en0` (Wi-Fi) | `en0` is the only bridgeable interface; a **real LAN lease** from the home router in 3.2 s, ping router and 1.1.1.1 |
| shared + `vmnet_enable_virtio_header_key` + `vmnet_enable_tso_key` | starts; `max_packet` rises from 1514 to 65550; every frame carries the 12-byte header (all zero on these small frames); DHCP/ARP/ping unchanged |
| network object, default config | starts, leases from the same 192.168.65 network as classic shared, NAT works |
| network object, our own MAC (`vmnet_allocate_mac_address_key` false) | works; the guest MAC is ours |
| `add_dhcp_reservation` | honoured: the reserved MAC got the reserved address |
| two interfaces on one network | ARP each other |
| interface on a second network | no ARP and no ping through the routers to the first: **isolated at L2 and L3** |
| `add_port_forwarding_rule` host:2299 → guest:22 | delivered for connections arriving on a real interface (tailnet address, and from the shell's own connects); **not** for this binary's connects to `127.0.0.1` or the host's own `en0` address |
| host → guest directly (`nc` to the guest's address on `bridge100`) | SYNs arrive; no forward is needed for the host to reach a guest |

- **`vmnet_network_configuration_set_ipv4_subnet` takes the gateway address, not the network
  address.** `192.168.211.1/24` works; `192.168.211.0/24` is accepted with `VMNET_SUCCESS` and the
  interface start then fails with a bare `VMNET_FAILURE` (1001). `vmnet_network_get_ipv4_subnet`
  likewise reports the gateway (`192.168.65.1`).
- **A pinned subnet is exclusive:** a second `vmnet_network_create` for a subnet a live network
  holds fails with 1001 until the first one is gone.
- **NECP drops host connects to a guest from our own binaries.** A `connect()` to the guest's
  address ends in `EHOSTUNREACH`, with `tcp drop outgoing … interface: bridge100` and `reason: NECP`
  in the unified log, and no frame reaches the bridge. That holds for `miniguest` and for a bare
  ad-hoc `connect()` binary with no entitlements alike, launched from the same shell where Apple's
  `/usr/bin/nc` (`com.apple.nc`) gets through. So the trigger is the binary's identity, not the
  entitlements or the vmnet handle, which is consistent with Local Network privacy exempting
  platform binaries. Whether a Developer ID app gets through after the user grants Local Network
  access is not measured. vmnet itself is unaffected (it is not a socket); this matters to any
  limina process that dials a guest directly, such as an SSH readiness probe.
- The host-only lease handed out DNS `100.100.100.100`, i.e. the host's resolver at the time
  (Tailscale's): bootpd passes the host's DNS through.

## Under a full-tunnel VPN (Tailscale exit node), measured 2026-10-08 on the dev Mac

`--hold <secs>` keeps the interface up and checks 1.1.1.1 every second with an ICMP echo (varying
sequence) and a UDP DNS query; every receive loop answers ARP for the guest's address.
`ext <ifname|default|follow> --hold <secs>` does the same on a network-object shared network whose
NAT uplink is set with `vmnet_network_configuration_set_external_interface`; `follow` reads the
host's route to 1.1.1.1 before every check (an `RTM_GET` on the routing socket) and, when it
changes, stops the interface, releases the network, and rebuilds both on the new uplink with the
same guest MAC.

**The breakage is Tailscale's "Allow local network access", not vmnet's NAT binding.** With that
option on, the exit node installs a static route for the vmnet subnet through the LAN router
(`192.168.65  192.168.13.1  UGSc  en0`) in place of the bridge's connected route. Guest traffic is
NATed out, and the replies are sent to the LAN router instead of the bridge.

| exit node | "Allow local network access" | shared-mode guest reaching 1.1.1.1 | route for the vmnet subnet |
|---|---|---|---|
| off | — | every check answered | `bridge100` |
| on | on | stops when the exit node comes up under a live network; a network created or rebuilt behind it answers for about 3 s, then ICMP and UDP stop for good | static, via the LAN router on `en0` |
| on | off | ICMP answered for the whole 66 s window, after a rebuild onto `utun7` | `bridge100` |
| on → off, network kept | on | stays broken | — |
| on → off, network rebuilt (`follow`) | on | answers at once | `bridge100` |

- `follow` caught every uplink change on its next check; the routing socket and
  SystemConfiguration's `State:/Network/Global/IPv4` `PrimaryInterface` named the same interface
  (`utun7` with the exit node, `en0` without) every time. A rebuild took 0.30–0.35 s, and the guest
  got the same address back each time, since bootpd keys its leases on the MAC.
- `set_external_interface` is accepted for `en0` and `utun7` and the network follows it, but it
  cannot help against the route above, which the VPN client installs after the network appears.
- UDP DNS to 1.1.1.1 failed throughout the exit-node-on, local-access-off window while ICMP got
  through. The host's own network was misbehaving in the same window (the user saw connections to
  remote services fail), and the host's own DNS was not checked, so that result is not counted
  either way.
- Bridged guests are untouched by the host's VPN because their traffic never enters the host's
  routing; they also never use the tunnel, which is a policy question for VPN users.
- Root was not available, so the pf NAT rules were not inspected.

Not measured here: throughput through a real virtio-net guest, other VPN clients (WireGuard, Cisco,
Zscaler), Internet Sharing on (1009), sleep/wake and Wi-Fi changes, pf state left behind
(`scrub … no-df`), coexistence with Apple `container`.
