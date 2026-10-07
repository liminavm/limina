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
