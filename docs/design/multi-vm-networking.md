# Multi-VM & Bridged Networking for limina

Status: **proposal; the default is decided** · Host: macOS 26+ (Apple Silicon) · Audience: limina
maintainer

Every networking feature here runs without root or a restricted Apple entitlement. **A VM's NIC is
on vmnet shared mode by default** (Apple's `vmnet.framework`, driven from the worker through a
native libkrun backend with the unrestricted `com.apple.security.virtualization` entitlement).
**When vmnet cannot serve — it fails to start, or a host VPN breaks its NAT — the NIC moves
automatically to `limina-netd`**, a per-VM user-mode NAT process of our own that talks to the
worker through shared-memory rings. `limina-netd` replaces gvproxy, which limina ships today and
retires once `limina-netd` covers what it does.

Measured evidence: `spikes/vmnet-network-probe/RESULTS.md` (vmnet privilege, leases, network
objects, VPN behaviour) and `spikes/net-bench/RESULTS.md` (gvproxy and vmnet throughput and CPU).
Background: `docs/research/07-networking.md`. Code: `crates/limina/src/gateway.rs` (gvproxy
lifecycle), `crates/limina-vmm/src/krun/mod.rs` (`net_device`), libkrun's
`src/devices/src/virtio/net/vmnet.rs`.

## 1. Goals

- **(a) Managed, isolatable networks.** Multiple concurrent VMs; VMs on the *same* limina network
  see each other; VMs on *different* networks are mutually invisible.
- **(b) Bridged-to-LAN.** A VM attached to a physical interface (Wi-Fi or Ethernet) takes a LAN
  lease and is a first-class peer.
- **(c) No orphaned helpers.** Nothing a VM spawns outlives it.
- **(d) Works behind the user's VPN.** A host VPN coming up, going down or changing must not leave
  a VM without the Internet.

## 2. What exists and what is measured

**gvproxy (shipped, `gateway.rs`, to be retired).** One gvproxy per VM, listening on a vfkit-style
unixgram socket keyed on the supervisor pid; the worker attaches virtio-net to it (`UnixgramPath`,
vfkit framing, `NET_FLAG_CSUM_VALID`). DHCP leases the guest `192.168.127.2`; the host SSH forward
auto-allocates from 2222 up and moves at runtime through gvproxy's `forwarder/expose`/`unexpose`
API; MTU 65520 (`GVPROXY_MTU`) and a raised `GOGC` are the measured throughput settings. A watchdog
respawns a gvproxy that dies under a running VM. Orphans are covered three ways: clean-exit
teardown, an EOF death-pact watcher (`limina __reap-gateway`), and a startup sweep of pid-files
keyed on dead supervisors. Everything here that gvproxy provides, `limina-netd` must provide before
gvproxy goes (§3.2).

**vmnet (experimental, `--net-vmnet shared|host|bridged:<if>`).** libkrun's vmnet backend
(`NetDevice::new_vmnet`) starts the interface in the worker with the virtio-net header passed
through both ways, TSO when the guest takes TSO4 and TSO6, batched reads, and the guest's own MAC.
The L2 test `net_vmnet` boots a stock guest on shared mode and checks its lease, DNS, HTTPS and a
64 MiB SHA-256 copy each way. Bridged over Wi-Fi was checked by hand. Measured on a quiet M1, macOS
27.0.1, the same stock Fedora guest on each backend (`spikes/net-bench/RESULTS.md`):

| | gvproxy | vmnet shared |
|---|---|---|
| iperf guest → host | 13–15 Gbit/s | 35 Gbit/s |
| iperf host → guest | 7.2–7.5 Gbit/s | 16–17 Gbit/s |
| host CPU per Gbit/s, g→h / h→g (worker + gvproxy) | 23–35% / 50% | 6.6% / 14% |
| ssh, either way | ~1.6–1.75 Gbit/s (cipher-bound) | the same |
| RTT p50 / p99 | 120 / 165 µs | 90 / 154 µs, with 2–5 ms outliers |
| idle, quiet guest | ~0.65 CPU-s per 30 s | the same |
| extra memory | gvproxy ~94 MiB RSS | — |

What the vmnet spike established (macOS 26.6.2 unless noted; `spikes/vmnet-network-probe/`):

| | Result |
|---|---|
| Privilege | `com.apple.security.virtualization` unlocks every mode; `com.apple.security.hypervisor` alone fails every mode. The two coexist on one binary. |
| Shared (Apple NAT) | DHCP lease in ~50 ms, router, DNS proxy, Internet |
| Host-only | Lease, host reachable, no router by design |
| Bridged over Wi-Fi | A real LAN lease from the LAN's DHCP server, Internet through the LAN router (also on 27.0.1) |
| virtio-header + TSO mode | Max packet 65550, and every frame carries a 12-byte `virtio_net_hdr`; a read needs room for both (27.0.1) |
| Own MAC | Honoured by network objects and by classic mode (27.0.1) |
| Network-object API (macOS 26) | DHCP reservations honoured; two interfaces on one network see each other; a second network is isolated at L2 and L3; NAT works |
| Port forwards | Delivered for traffic arriving on a real interface (LAN, tailnet), not for host-local connects |
| Host → guest | The host reaches a guest's address on the bridge directly; no forward needed |

Not measured: a Developer ID + notarized build, the App Store sandbox, sleep/wake, Wi-Fi roaming,
Internet Sharing being on (`VMNET_SHARING_SERVICE_BUSY`), and pf state left behind after teardown
(apple/container #2335 reports a `scrub … no-df` rule breaking host IPv4 on some ISPs).

## 3. Architecture

### 3.1 A first-class `Network` abstraction

A named **Network** that VMs attach to — the shape lima and podman converged on.

```
Network {
    name:    String,              // stable id, e.g. "default", "lab-a"
    type:    Nat | Bridged | Host,
    subnet:  Option<Ipv4Net>,     // NAT/Host: omit → backend picks
    uplink:  Option<String>,      // Bridged: physical NIC, e.g. "en0"
}

NicAttachment {                   // one per VM virtio-net device
    network: NetworkRef,
    mac:     MacAddr,             // per-VM stable MAC
}
```

Each reference becomes one virtio-net device; libkrun's API takes N NICs, each with its own
backend and MAC. The per-VM MAC already exists: `vm.toml` stores one derived from the VM uuid
(`vmlib/schema.rs` `mac_for_uuid`). Every vmnet guest of a mode shares a segment, so the fixed vfkit
MAC gvproxy relied on cannot be the default there; without a MAC the supervisor picks a random
locally administered one.

A NAT NIC has two backends behind one virtio device: vmnet (§3.3) while it serves, `limina-netd`
(§3.2) when it does not, switched by the worker (§3.5).

### 3.2 `limina-netd`: the user-mode NAT

**One `limina-netd` per VM, a separate process, the NIC's fallback and the floor under vmnet.** It
terminates the guest's traffic and re-originates it on ordinary host sockets, so to macOS it is an
app making connections: it follows the host's routing and DNS, VPN tunnels included, needs no
entitlement, and leaves no host network state. A separate process because it parses untrusted
traffic from both sides: a crash costs the VM its network, not the VM, and a `sandbox_init` profile
can hold it to sockets with no file access. The supervisor starts it with the VM and ties it to the
VM's life the way gvproxy is tied today (goal (c)); it runs from boot so that a switch (§3.5) is
immediate.

**Data path: two rings and two doorbells, no socket hop.** The worker keeps processing the guest's
virtqueues and hands frames to `limina-netd` through two single-producer/single-consumer rings in
one shared memory entry: TX (worker → netd) and RX (netd → worker), each frame with its
`virtio_net_hdr` in front. The rings are `limina-ring` (`crates/limina-ring`): variable-length
records, a doorbell needed only when the consumer has parked, and the publish-then-recheck park
handshake of the Metal transport's command ring (`crates/limina-gpu-wire/src/ring.rs`), so neither
side makes a system call per frame under a steady stream. The memory is a Mach memory entry
(`mach_make_memory_entry_64`) handed over in a Mach reply, as for the Metal transport
(`spikes/replayer-ipc/`). The doorbell has to wake netd's kqueue loop, which also waits on its
sockets, so it is a kqueue-visible object (a Mach port under `EVFILT_MACHPORT`, or a pipe) rather
than the Metal transport's semaphore; it fires only on a park, so a system call per wake is cheap.
Each consumer trusts nothing the other side wrote: lengths are checked against what was published
before a byte is copied. One copy each way (guest buffer ↔ ring), against the unixgram path's two
plus a system call per frame.

**The worker's half lives in limina, not in libkrun.** libkrun's delta is a hook: its
`NetBackend` trait exported and an external backend a `NetDevice` can take, the way limina-vmm
already serves the Metal transport through libkrun's `GpuExtension`. limina-vmm implements the
netd backend over the rings, and the switching backend of §3.5 that wraps vmnet and netd; neither
the ring nor the switch policy enters the fork.

**Offloads.** Guest TSO segments arrive whole and are just payload to netd, which never
re-segments them. Towards the guest, netd builds 64 KiB frames with a GSO header and marks them
checksum-valid (it built them), so the guest's MTU stays 1500 and the large-MTU workaround gvproxy
needs goes away.

**TCP: passt-style translation, no TCP stack of its own.** Each guest connection maps onto one host
socket, and netd keeps the guest's view consistent by translating sequence numbers; the host's TCP
does the congestion control, retransmission and windowing towards the far end:

- *Guest → host:* the payload of a guest segment is written to the host socket, never more than the
  room the socket's write event reports (a non-blocking write larger than the free space mostly
  fails with `EAGAIN` rather than writing short). netd ACKs the guest as the peer ACKs, read from
  `TCP_CONNECTION_INFO`'s `tcpi_snd_sbbytes`; the guest's own retransmission covers the rest.
- *Host → guest:* **read-and-retain.** netd reads the socket once into a per-connection buffer the
  size of the guest's window, sends from it to the guest as large segments, and frees it as the
  guest ACKs, resending from it when the guest's ACKs stall. passt peeks the receive queue instead
  and keeps nothing; macOS has no `SO_PEEK_OFF`, so every peek re-copies what is in flight, which
  made it CPU-bound at 26–29 Gbit/s at two to three times the CPU per byte, while read-and-retain
  costs what reading straight into the guest frame costs and, unlike that, can resend
  (`spikes/netd-tcp-translate/RESULTS.md`).
- *Connection life:* guest SYN → non-blocking `connect()`; the host socket's FIN and RST map onto
  the guest's, and the reverse.
- *Events:* kqueue's event data carries the readable bytes and the writable room, so the data path
  needs no `getsockopt` but the ACK count.

**The rest of the stack:**

- **ARP and DHCP** answered locally, a stable address per MAC.
- **DNS** answered at the gateway through the host's resolver, so a VPN's split DNS applies.
- **UDP** as a flow table onto connected host sockets, with idle timeouts.
- **ICMP echo** through macOS's unprivileged ICMP sockets (`SOCK_DGRAM`, `IPPROTO_ICMP`).
- **Port forwards** (the SSH forward among them) as host listening sockets that open connections
  into the guest; the runtime forward API moves with them.
- **IPv6** later.

**Enhanced tier.** A custom guest NIC driver writes frames straight into a guest-visible arena and
rings a doorbell page, as `virtio-gpu-limina` does for the Metal transport (an MMIO trap, ~1 µs):
netd reads the guest's frames where the guest wrote them and the worker's copies go away, while
netd sees only that arena, never guest RAM at large. Further out, socket-level offload (the guest's
`connect`/`send` forwarded to netd, skipping TCP in both guest and host; libkrun's TSI is prior art,
with its limits around listening and raw sockets). Both are additive: the stock guest's virtio-net
path is the floor.

**Multi-VM.** One netd per VM gives each fallen-back VM its own island, the same isolation gvproxy
gives today. VMs that must see each other do so on a vmnet Network (§3.3); a VM that has fallen back
to netd loses its Network peers until it moves back.

### 3.3 vmnet in the worker

**The worker (`limina-vmm`) holds the vmnet interface, through the native vmnet backend in
libkrun's virtio-net.** The interface lives in the worker process and dies with it, so there is no
helper and nothing to orphan. Public API only: batched `vmnet_read`/`vmnet_write`, no private
syscalls, no vendored C.

The backend starts the interface with `vmnet_enable_virtio_header_key`, and with
`vmnet_enable_tso_key` when the guest acked GUEST_TSO4 and GUEST_TSO6 (vmnet's one switch covers
both directions and both families), so the `virtio_net_hdr` passes straight between the guest's
virtqueues and vmnet. Mechanism in libkrun, upstreamable: the backend takes a mode, a bridged
interface name and the guest MAC; a network object is the next input (Phase 6). Policy in limina:
which Network, which mode, uplink tracking (§3.4), VPN detection and the switch to netd (§3.5).

**Modes onto Networks:**

- **NAT (shared) and Host:** one vmnet network per limina Network, created with the macOS 26
  network-object API and handed to each attached worker (`vmnet_network_copy_serialization` →
  `vmnet_network_create_with_serialization`). DHCP reservations
  (`vmnet_network_configuration_add_dhcp_reservation`) give each VM's MAC a stable address; a
  separate network per Network gives isolation. Cross-process serialization is not yet measured.
- **Bridged:** `VMNET_BRIDGED_MODE` with `vmnet_shared_interface_name_key`; the UI lists
  `vmnet_copy_shared_interface_list`. A bridged guest's traffic goes to the LAN without entering the
  host's routing, so a host VPN neither breaks it nor carries it; the UI says so. Bridged is an
  explicit choice, never a fallback.

**API facts the code must respect:**

- `vmnet_read` refuses (`VMNET_INVALID_ARGUMENT`) a buffer smaller than `vmnet_max_packet_size_key`
  plus the 12-byte header: the reported maximum leaves the header out.
- `vmnet_network_configuration_set_ipv4_subnet` takes the **gateway** address (`192.168.211.1`),
  not the network address; `.0` returns success and the interface start then fails with a bare
  `VMNET_FAILURE`. A non-private subnet is refused at `vmnet_network_create`.
- A pinned subnet is exclusive while a network holds it; a second create fails.
- vmnet's DHCP server keys leases on the MAC, so a rebuilt network hands the same VM the same
  address.
- Port-forward rules (`vmnet_network_configuration_add_port_forwarding_rule`) apply to traffic
  arriving on a real interface: they publish a VM to the LAN or tailnet. Host-local access goes to
  the guest's address directly.
- `vmnet_network_create` can block indefinitely on macOS 27 (apple/container #2275): call it off
  the boot path with a timeout.

**Host → guest.** A process that dials a guest's vmnet address is subject to Local Network
privacy (NECP): our ad-hoc binaries are dropped with `EHOSTUNREACH`, while Apple's platform
binaries (`/usr/bin/nc`, `/usr/bin/ssh`) get through, also when one of our binaries spawns them.
Whether a Developer ID app passes after the user grants Local Network access is unmeasured.

### 3.4 Uplink tracking

A shared network NATs out of one uplink, chosen when it is created (vmnet's default: the route-table
default; `vmnet_network_configuration_set_external_interface` to choose). No call changes a live
network's uplink. The worker therefore watches the host's route (routing-socket `RTM_GET`, or
SystemConfiguration's `State:/Network/Global/IPv4` `PrimaryInterface` — the two agreed on every
change the spike saw; it polled both once a second, while the product would subscribe to change
notifications, which is untested) and on a change stops the interface, releases the network,
and rebuilds both on the new uplink with the same MAC. Measured: 0.30–0.35 s, same guest address,
next-check detection. The guest sees a link reset; open connections drop. When the new uplink is
one vmnet cannot serve (§4), the worker switches to netd instead (§3.5).

### 3.5 Switching between vmnet and netd

**Automatic, in the worker, under one virtio device and one MAC.** The NIC's backend is limina's
switching backend (§3.2), which owns libkrun's vmnet backend and the netd backend and swaps which
one the virtqueues talk to:

- **To netd** when vmnet cannot serve: the interface fails to start (privilege, `SERVICE_BUSY`, a
  start or create that does not answer within its timeout), the vmnet subnet's route no longer
  points at the VM's bridge (the Tailscale local-access hijack, §4), or the uplink is a tunnel
  interface (under which forwarded UDP was lost, §4).
- **Back to vmnet** when the condition has cleared and stayed clear for a hold-off, so a flapping
  VPN does not flap the guest.

The guest has to notice that its subnet changed, so the worker drops the link for a moment: virtio-
net's link status (`VIRTIO_NET_F_STATUS` and a config-change interrupt, which libkrun's net device
does not offer yet) takes the carrier away and back, and the guest's DHCP client takes a lease on
the new side. A stock NetworkManager does this on its own; open connections drop, as on an uplink
rebuild. Every switch is logged and shown in the UI with its reason. Port forwards follow the
backend: on netd the SSH forward is a netd listening socket; on vmnet the guest is reached at its
own address.

## 4. Host VPNs and other software

vmnet shared mode's NAT is the host's pf, so a host VPN can break it in ways user-mode NAT is immune
to. Measured with a Tailscale exit node (`spikes/vmnet-network-probe/RESULTS.md`):

| Exit node, "Allow local network access" | Shared-mode guest |
|---|---|
| on, on | Loses the Internet: Tailscale installs a static route for the vmnet subnet through the LAN router in place of the bridge's route, within ~2–3 s of the network appearing. A subnet in each private range was taken alike (192.168.65, 10.211.0, 172.30.211). It clears only when the bridge goes away. |
| on, off | ICMP, TCP and DNS through vmnet's gateway proxy work; forwarded UDP on any port gets no reply, while the host's own UDP works. |
| on, either, uplink pinned to `en0` | Everything works, presumably by bypassing the tunnel. Not a fix: it leaks around the VPN. |

Uplink tracking (§3.4) does not help with the route: the VPN client installs it after the network
exists, and replacing it needs root. Other clients fail in their own ways (research, not measured
here): the NAT staying on the physical uplink after a VPN comes up (apple/container #1307, #1519;
UTM #7207), VPN clients' own pf rules blocking the subnet (Cisco, #1519), routes to the VM subnet
moved into the tunnel (tailscale #18653), and starting vmnet resetting Cisco/Ivanti (#762). Docker
Desktop, Lima, Podman and UTM keep a user-mode path largely for VPN users.

**So:** the worker detects these conditions (§3.5) and moves the NIC to netd, whose traffic is the
host's own socket traffic and goes wherever the VPN sends it. If vmnet cannot start at all, the VM
comes up on netd: vmnet never gates the floor.

## 5. Two-tier mapping

The tiers are about the *guest*, and both backends serve a stock guest: an unmodified Fedora runs
NetworkManager DHCP against vmnet or netd alike, with no limina components, and survives a switch
between them as a link change. `limina-netd` is what always works (no entitlement, immune to VPNs).
The enhanced tier adds the arena NIC driver and, later, socket offload (§3.2); `limina-agent` adds
niceties (reporting the guest's address, hostnames). None is required.

## 6. Phases (RED-first, bisectable)

Each phase ships on its own and is tested against the shipped binaries (`crates/limina-test`).

- **Phase 1 — native vmnet backend, then the measurement (done).** §2.
- **Phase 2 — the TCP translation spike and the ring crate (done).** The spike
  (`spikes/netd-tcp-translate/`) chose read-and-retain over peeking and settled the socket
  bookkeeping (§3.2); `limina-ring` is the generic core of the Metal transport's ring, which keeps
  its own until it is moved onto the core (`docs/hardening-backlog.md`).
- **Phase 3 — `limina-netd` with gvproxy's feature set.** netd, libkrun's external-backend hook,
  limina-vmm's netd backend, and an explicit flag to select it. *RED:* the existing net tests (lease, SSH forward, per-VM MAC, two
  VMs in parallel) pass on netd; their oracles move off gvproxy's packet log.
- **Phase 4 — vmnet shared by default, with the switch.** Uplink tracking (§3.4), the switch and
  link flap (§3.5), libkrun's link status. *RED:* a VM boots on vmnet by default; a forced
  trigger (a test lever) moves a running guest to netd, which leases and reaches the Internet, and
  back; a vmnet start that fails leaves the VM on netd.
- **Phase 5 — retire gvproxy.** `gateway.rs`'s gvproxy half, its bundling, its watchdog and reaper,
  its docs.
- **Phase 6 — `Network` model, per-VM MACs, vmnet network objects.** `limina net` CLI; one network
  object per Network with reservations; forwards per VM. *RED:* two VMs on two Networks, mutually
  invisible; two VMs on one Network ping each other.
- **Phase 7 — bridged as a product feature.** Interface picker, `NOT_AUTHORIZED`/`SERVICE_BUSY`
  surfaced. *RED:* the guest takes a LAN lease and is reachable from another LAN host over Wi-Fi
  and Ethernet.
- **Phase 8 — enhanced-tier arena NIC.** The guest driver and netd reading the arena (§3.2).

## 7. Decision: vmnet shared by default, `limina-netd` as the fallback

Decided on Phase 1's measurements (§2): vmnet moves 2.3–2.5× the bytes at about a third of the host
CPU per byte, gives VMs real addresses the host reaches directly, and gives multi-VM networks
through network objects. Its weakness is §4, host VPNs, and that is what the automatic switch to
`limina-netd` covers. gvproxy goes: what it does, netd does with no socket hop, no Go runtime and no
large-MTU workaround, and netd's TCP is passt-style translation rather than a full stack.

## 8. Open questions

- **netd end to end:** the TCP spike measured netd's share on loopback alone; throughput through the
  worker, the ring and a guest, many connections at once, and UDP are measured once netd exists.
- **The tunnel trigger:** UDP loss was measured under one client; switching on any tunnel uplink is
  the conservative rule, and other clients (WireGuard, Cisco, Zscaler) may need finer ones.
- **Cross-process network objects:** `vmnet_network_copy_serialization` across workers is the basis
  of Phase 6 and is not yet exercised.
- **Forwarded UDP under an exit node:** where it is lost (pf NAT on `utun`, or the VPN client) is
  unknown without root.
- **vmnet's open measurements:** one host → guest run with 2067 retransmits (its twin had 3), and
  2–5 ms latency outliers.
- **Distribution:** whether a Developer ID + notarized, or App Store, build gets the same vmnet
  behaviour (`docs/design/distribution.md`); re-run `spikes/vmnet-network-probe` in each.
- **macOS 27:** the network-object API, and the reported indefinite `vmnet_network_create`.
- **Coexistence:** Apple `container` and other vmnet users on the same host; subnet collisions with
  the LAN; pf state after teardown.
- **Wi-Fi roaming and sleep/wake** on vmnet and netd.

## 9. Tailscale integration (fleet-wide, no per-VM config)

> **Premises changed by §7.** Option A below embeds Tailscale's Go netstack in a gvproxy-based
> `limina-networkd`; with gvproxy retired for `limina-netd` (Rust, passt-style, one per VM) that
> host is gone, and A has to be re-planned or dropped. Option B (host `tailscaled` over a vmnet
> bridge) now covers the default backend. C is unaffected.

> Options **A** and **C** below work on user-mode NAT; Option **B** (host-side `tailscaled` over a
> vmnet bridge) waits on the vmnet tier (Phases 1 and 3), which needs no privilege. Researched and
> adversarially verified 2026-06-25.

**Goal.** Offer tailnet connectivity *through* the limina infrastructure — VMs reachable over the
user's tailnet (and reaching it) **without configuring each guest** — rather than installing and
authenticating `tailscaled` in every VM by hand.

**The mechanism.** The "no per-VM config" primitive is the Tailscale **subnet router**: one tailnet
node runs `tailscale up --advertise-routes=<Network CIDR>`, the route is approved (admin or an
`autoApprovers` ACL), and any peer with `--accept-routes` reaches the VMs by IP — nothing installed
in the guest (kb/1019). This is exactly fly.io's model (one `tailscale-router` node advertises an
org's whole 6PN; no agent in any microVM).

The limina-specific opportunity: **gvproxy / `limina-networkd` is itself a gVisor userspace netstack
— the same `gvisor.dev/gvisor/pkg/tcpip` stack Tailscale is built on, same language (Go).** So the
elegant path is not an external router bolted on the side; it is to **embed a Tailscale node inside
`limina-networkd`** and bridge the tailnet into the same userspace L2 segment the VMs already share.
Each Network optionally *becomes* one tailnet subnet-router node; VMs stay zero-config. One node per
Network maps 1:1 onto "a Network is an isolation group."

### 9.1 Two verified mechanism facts (they shape the build)

- **`tsnet` alone cannot advertise routes.** The public `tsnet.Server` exposes `Hostname`/`AuthKey`/
  `Ephemeral`/`AdvertiseTags`/OAuth/`ControlURL` but **no `AdvertiseRoutes`** — it publishes the app
  as a *single endpoint node*, not a subnet. The real lever is one level down: `wgengine/netstack`'s
  **exported** `Impl.ProcessSubnets` (doc comment: "whether netstack should handle incoming traffic
  destined to non-local IPs, i.e. whether it should be a subnet router"). (`tsnet`'s stable
  `Listen`/`Dial` is still useful for a *separate* "expose a few named services over the tailnet"
  mode — but that is not a subnet router.)
- **The gvproxy dial-path catch → a vendored patch.** A stock netstack subnet router dials its
  targets through the **host kernel** (`forwardTCP` → `net.Dialer`, `forwardUDP` → `net.ListenUDP`),
  so it cannot see a subnet that lives only inside gvproxy's userspace netstack (no host route). The
  TCP redirect hook `Impl.forwardDialFunc` is **unexported and "currently only used in tests," and
  UDP has no override at all.** Redirecting VM dials into our shared userspace segment therefore
  **requires a pinned, maintained patch to Tailscale** (TCP + UDP), not just configuration —
  acceptable under "own the stack," but treat it as a patch surface, not a public contract.

So Option A = **embedded `wgengine/netstack` with `ProcessSubnets=true` + a patched dial path into
the gvproxy segment**, driven by the Go core of `limina-networkd` (not plain `tsnet`, not Rust).

### 9.2 Options, ranked (under unprivileged-first)

| # | Option | Per-guest config? | Stock guest? | Per-VM identity/MagicDNS? | Effort | Privilege | Status |
|---|--------|:---:|:---:|:---:|:---:|:---:|---|
| **A** | Embedded node in `limina-networkd` (`ProcessSubnets` + patched dial into the gvproxy L2 segment) | **None** | **Yes** | No (subnet-router) | High (vendored TS patch) | **None** | **Strategic target** (needs re-planning, see the note above) |
| **C** | Agent-injected per-VM `tailscaled` (ephemeral *tagged* auth key over vsock) | None (agent injects) | No (needs agent) | **Yes** | Medium | None on host; guest-internal TUN only | **Near-term win** |
| **B** | Host-side `tailscaled` subnet router over a vmnet `bridge100` (`--advertise-routes=<CIDR>`) | None | Yes (on a vmnet Network) | No | Low | vmnet (`com.apple.security.virtualization`) | **After Phase 4** (vmnet by default) |

- **Why A reaches stock guests and B can't:** the default gvproxy `/24` exists only inside gvproxy's
  userspace netstack — there's nothing host-routable for an external router to advertise. A (inside
  the daemon that owns that netstack) is the only path that brings the tailnet to the **zero-privilege
  NAT floor a stock guest gets by default**, satisfying the two-tier guarantee. B works only where
  vmnet has produced a real bridge interface — so it follows Phase 4.
- **Why C is the near-term win:** it needs **no Tailscale source patch** — it runs stock `tailscaled`
  in the guest with an ephemeral, pre-tagged auth key delivered over the existing vsock control plane,
  and it is the *only* option that returns true per-device identity (MagicDNS, per-device ACL tags,
  Taildrop, real source IPs). Host-unprivileged; the guest's own `/dev/net/tun` (kernel-mode, most
  transparent) or `--tun=userspace-networking` (no guest TUN) costs no *host* privilege. Enhanced-tier
  by nature: a guest with no `limina-agent` can't be auto-provisioned, so detect it **per-feature**
  (Network has Tailscale enabled **and** guest has the agent **and** `tailscale` present) — additive,
  never gating the baseline.

### 9.3 Tradeoffs to accept

- **Subnet router (A/B) = reachability-by-IP, never per-device identity.** No MagicDNS name, no
  per-device tags/ACLs (ACLs are by CIDR), no Taildrop. C is the answer when identity is required.
- **Userspace/netstack forwarding is not transparent L3** (kb/1177): it terminates TCP/UDP and
  re-originates — **only TCP/UDP + reconstructed ping**, no arbitrary ICMP (traceroute breaks), no
  SCTP, and a CPU-bound throughput penalty. For A this is a **double-netstack hop** (Tailscale
  netstack → redirect → gvproxy netstack) — **benchmark on M1 before committing**; watch MTU stacking
  (Tailscale defaults to 1280 under an already-virtual path → measure end-to-end MSS, maybe clamp).
- **Overlapping `192.168.127.0/24`** across Networks (and across limina hosts on one tailnet) collide
  → use **4via6** per-Network site IDs (`tailscale debug via <siteID> <cidr>`, v1.24+), or allocate
  unique per-Network CIDRs up front. Subnet-routed VMs get no MagicDNS, so limina must surface
  friendly per-Network reachable names (a `*.limina` split-DNS proxy, fly's `*.internal` style).
- **SNAT is forced on macOS** (`--snat-subnet-routes=false` is Linux-only) → VMs appear as the
  router's IP to the tailnet.
- **Stateful filtering is Linux/nftables-only** (open FR upstream suggests netstack subnet routers
  may not stateful-filter): **enforce isolation via Tailscale ACLs + limina's Network boundaries; do
  not assume inherited kernel-mode inbound-drop.**
- **Headless auth & secret custody:** provision with **tagged + ephemeral** auth keys (or tagged
  OAuth-minted), self-approve via `autoApprovers`. Key/secret custody on the host is a real surface;
  ephemeral keys auto-clean dead nodes but watch key-expiry for a *persistent* per-Network router.
- **Control plane:** consider offering **Headscale** (BSD-3, self-hosted, stock-client-compatible) so
  limina can present a zero-SaaS-account default.

### 9.4 Recommendation & mapping to the `Network` abstraction

Make Tailscale a **per-Network opt-in** (`--tailscale` flag / a field on the `Network` type), **never
on by default** — it changes the security boundary and needs explicit consent.

1. **Near-term — Option C** (agent-injected per-VM node). Lowest effort that ships real value on the
   unprivileged path, reuses the vsock control plane + `limina-agent` we're already building, and
   gives full device identity. Enhanced-tier (agent-bearing guests).
2. **Strategic — Option A** (embedded subnet router in `limina-networkd`). The honest "Tailscale
   through the infrastructure" answer: zero in-guest components, covers **stock** guests, lives in the
   Go daemon, and the `forwardDialFunc`/UDP patch is exactly the small pinned dependency patch the
   own-the-stack tenet exists for. It assumed a gvproxy-based `limina-networkd`
   with a multi-peer segment, which §7 drops.
3. **After Phase 4 — Option B** (host `tailscaled` over vmnet). Cheap once VMs sit on a vmnet
   Network; it covers only VMs attached to one. Measure it against §4 first: the host `tailscaled`
   is the same client whose local-access exclusion takes vmnet subnets away from their bridge
   under an exit node, and whether advertising a vmnet subnet trips the same logic is unmeasured.

**Rollout addendum (extends §6, unprivileged-first):**
- **Phase T1 — Tailscale opt-in, Option C:** `--tailscale` on a Network; `limina-agent` brings up
  `tailscaled` in agent-bearing guests via an ephemeral tagged key over vsock; route/identity via
  per-VM node. *RED:* an agent guest on a `--tailscale` Network is reachable by its MagicDNS name from
  another tailnet peer; a stock guest on the same Network is unaffected.
- **Phase T2 — Tailscale Option A (embedded subnet router):** after `limina-networkd`; vendor the
  Tailscale netstack patch (`ProcessSubnets` + TCP/UDP dial redirect into the Network's segment);
  one node per Network, 4via6 for overlapping CIDRs. *RED:* a fully **stock** guest on a `--tailscale`
  Network is reachable by IP over the tailnet with zero in-guest components; two Networks stay
  isolated. Benchmark the double-netstack path first.

### 9.5 Open questions / risks

- **The `forwardDialFunc` patch surface** is the most fragile piece (unexported + test-only; UDP has
  no hook). Decide: drive `LocalBackend`+`wgengine`+`netstack.Impl` directly, or fork `tsnet` to
  expose `ProcessSubnets` + a dial override? Prototype both; pin the version.
- **Performance on M1:** benchmark the double-netstack path (web/SSH/file-transfer); find where it
  caps; decide MTU/MSS clamping.
- **Inbound isolation in embedded netstack mode:** confirm no relied-upon inbound-drop is missing;
  enforce via ACLs + Network boundaries.
- **Auth-key / OAuth lifecycle & custody** on the host (storage, scoping, rotation, expiry for
  long-lived routers); for C, deliver over vsock without leaking the key into the guest beyond
  `tailscaled`'s use; can route auto-approval be fully zero-touch via the API?
- **Identity model decision:** is per-Network identity (A, one node) enough for limina's users, or is
  per-VM identity (C) a must-have for enough workflows that it should be co-primary, not just opt-in?
