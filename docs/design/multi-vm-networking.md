# Multi-VM & Bridged Networking for limina

Status: **the default and bridged are built (Phases 1–5, 7); networks as objects and the arena NIC to come** · Host: macOS 26+ (Apple Silicon) · Audience: limina
maintainer

Every networking feature here runs without root or a restricted Apple entitlement. **A VM's NIC is
on vmnet shared mode by default** (Apple's `vmnet.framework`, driven from the worker through a
native libkrun backend with the unrestricted `com.apple.security.virtualization` entitlement).
**When vmnet cannot serve — it fails to start, or a host VPN breaks its NAT — the NIC moves
automatically to `limina-netd`**, a per-VM user-mode NAT process of our own that talks to the
worker through shared-memory rings.

Measured evidence: `spikes/vmnet-network-probe/RESULTS.md` (vmnet privilege, leases, network
objects, VPN behaviour) and `spikes/net-bench/RESULTS.md` (gvproxy and vmnet throughput and CPU).
Background: `docs/research/07-networking.md`. Code: `crates/limina-vmm/src/krun/mod.rs`
(`net_device`), `switch.rs`, `uplink.rs` and `lease.rs` beside it, `crates/limina-netd`,
`crates/limina/src/forward.rs` (the SSH forward), `crates/limina/src/uplinks.rs` (the interfaces a
NIC can be bridged onto), libkrun's `src/devices/src/virtio/net/vmnet.rs`.

## 1. Goals

- **(a) Managed, isolatable networks.** Multiple concurrent VMs; VMs on the *same* limina network
  see each other; VMs on *different* networks are mutually invisible.
- **(b) Bridged-to-LAN.** A VM attached to a physical interface (Wi-Fi or Ethernet) takes a LAN
  lease and is a first-class peer.
- **(c) No orphaned helpers.** Nothing a VM spawns outlives it.
- **(d) Works behind the user's VPN.** A host VPN coming up, going down or changing must not leave
  a VM without the Internet.

## 2. What exists and what is measured

**gvproxy (retired in Phase 5)** is the baseline the measurements below compare against: a Go
user-mode NAT per VM (gvisor-tap-vsock), on a vfkit-style unixgram socket, at an MTU of 65520.

**vmnet (`--net-vmnet shared|host|bridged:<if>` alone, or under the switch with `--net`).**
libkrun's vmnet backend (`Vmnet`, which the worker builds through libkrun's external-backend hook
so it can read the guest's lease, §3.3) starts the interface in the worker with the virtio-net
header passed through both ways, TSO when the guest takes TSO4 and TSO6, batched reads, and the
guest's own MAC. The L2 test `net_vmnet` boots a stock guest on shared mode and checks its lease,
DNS, HTTPS and a 64 MiB SHA-256 copy each way; `net_bridged` boots one bridged onto the host's
uplink, flat and managed, and reaches it at the LAN address the supervisor reports. Measured on a quiet M1, macOS
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
| Bridged onto a missing interface | No error: `vmnet_start_interface` never answers, and the worker's 10 s wait ends the start (27.0.1) |
| Bridgeable interfaces | `vmnet_copy_shared_interface_list` answers an unentitled process; on a Wi-Fi-only laptop it lists `en0` alone, not the Thunderbolt ports (27.0.1) |
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
MAC cannot be the default there: a flat `--net` run that names none takes one derived from its boot
disk's path, so it keeps its lease from boot to boot (vmnet's DHCP server holds a lease for a day,
and the pool is one /24 for the whole host).

A NAT NIC has two backends behind one virtio device: vmnet (§3.3) while it serves, `limina-netd`
(§3.2) when it does not, switched by the worker (§3.5).

### 3.2 `limina-netd`: the user-mode NAT

**One `limina-netd` per VM, a separate process, the NIC's fallback and the floor under vmnet.** It
terminates the guest's traffic and re-originates it on ordinary host sockets, so to macOS it is an
app making connections: it follows the host's routing and DNS, VPN tunnels included, needs no
entitlement, and leaves no host network state. A separate process because it parses untrusted
traffic from both sides: a crash costs the VM its network, not the VM, and once it holds its
descriptors it enters a `sandbox_init` profile that allows making and using sockets and nothing
else, bar the metadata of the `/var` symlink on the way to mDNSResponder's socket
(`crates/limina-netd/src/sandbox.rs`, each allowance found necessary by removing it). It runs from
boot so that a switch (§3.5) is immediate.

**The worker spawns its netd, and nothing else can reach it.** The channel is one Unix stream
socketpair: the worker keeps one end and hands netd the other as an inherited descriptor
(`limina_netd_wire`), so there is no path or name another process could open and nothing on disk
a crash could leave behind. The worker creates the shared memory as a POSIX shared-memory object
under a random name it unlinks straight after creating it, before sizing it, and sends the
descriptor over the socket. EOF on the socket is the peer's death: netd exits when the worker
goes (goal (c), with no reaper), and the worker starts a fresh netd on a fresh channel when netd
goes, through libkrun's reconnect path, so a netd crash costs open connections, not the VM's
network. The guest keeps its address across the restart, since netd leases its one address.

**Data path: two rings and two doorbells, no socket hop.** The worker keeps processing the guest's
virtqueues and hands frames to `limina-netd` through two single-producer/single-consumer rings in
one shared memory entry: TX (worker → netd) and RX (netd → worker), each frame with its
`virtio_net_hdr` in front. The rings are `limina-ring` (`crates/limina-ring`): variable-length
records, a doorbell needed only when the consumer has parked, and the publish-then-recheck park
handshake of the Metal transport's command ring (`crates/limina-gpu-wire/src/ring.rs`), so neither
side makes a system call per frame under a steady stream. The doorbell is a byte on the
socketpair, which both netd's kqueue loop and libkrun's net worker wait on with everything else;
it is written only when the peer has parked, so a system call per wake is cheap. Each consumer
trusts nothing the other side wrote: lengths are checked against what was published before a
byte is copied. Through libkrun's `NetBackend`, a frame is copied twice in the worker (guest
memory ↔ the net worker's frame buffer ↔ ring) and never by the kernel, against the unixgram
path's two kernel copies plus a system call per frame; reaching guest memory from the ring
directly would save one more and needs a wider libkrun interface.

**The worker's half lives in limina, not in libkrun.** libkrun's delta is a hook: its
`NetBackend` trait exported and an external backend a `NetDevice` can take, the way limina-vmm
already serves the Metal transport through libkrun's `GpuExtension`. limina-vmm implements the
netd backend over the rings, and the switching backend of §3.5 that wraps vmnet and netd; neither
the ring nor the switch policy enters the fork.

**Offloads.** netd negotiates checksum offload only: the guest's frames stay within its 1500-byte
MTU, and netd's segments towards it are MSS-sized with full checksums. Accepting the guest's TSO
segments whole, and building 64 KiB frames with a GSO header towards it, would cut the per-frame
work on both sides and are the next step for throughput (§8).

**TCP: passt-style translation, no TCP stack of its own.** Each guest connection maps onto one host
socket, and netd speaks just enough TCP on the guest's side to carry the bytes; the host's TCP
does the congestion control, retransmission and windowing towards the far end
(`crates/limina-netd/src/tcp.rs`):

- *Guest → host:* netd acknowledges the guest's bytes as they land in a 256 KiB per-connection
  buffer, advertises its free space as the window (scaled when the guest offers scaling), and
  writes it to the host socket in pieces no larger than the room the socket's write event reports
  (a non-blocking write larger than the free space mostly fails with `EAGAIN` rather than writing
  short). A window the guest had all but run out of is reopened with an unprompted ACK.
- *Host → guest:* **read-and-retain.** netd reads the socket into a 256 KiB per-connection buffer,
  sends from it within the guest's window, and frees it as the guest ACKs; a full buffer stops the
  reads, and the host's flow control takes over. passt peeks the receive queue instead and keeps
  nothing; macOS has no `SO_PEEK_OFF`, so every peek re-copies what is in flight, which made it
  CPU-bound at 26–29 Gbit/s at two to three times the CPU per byte, while read-and-retain costs what
  reading straight into the guest frame costs and, unlike that, can resend
  (`spikes/netd-tcp-translate/RESULTS.md`). Recovery is go-back-N on a 200 ms timeout doubling to
  5 s (ten without progress reset the connection), one resend per third duplicate ACK, and NewReno
  partial-ACK resends. On the real link netd never loses a segment: one the guest's ring has no
  room for is not counted as sent and goes a millisecond later.
- *Connection life:* guest SYN → non-blocking `connect()`, and the SYN-ACK only once it completes
  (a refusal, an unroutable destination or the gateway itself is reset; an unanswered connect is
  left to the guest's SYN retries); FIN and RST map onto each other both ways.
- *Events:* kqueue's event data carries the readable bytes and the writable room, so the data path
  needs no `getsockopt`. Both filters are level-triggered, so host sockets are armed one-shot.

**The rest of the stack:**

- **ARP and DHCP** answered locally: the guest gets `192.168.127.2`, whatever its MAC (the vfkit
  addressing: the gateway at `.1`).
- **DNS** answered at the gateway through the host's resolver (`DNSServiceQueryRecord`), so a
  VPN's split DNS applies. With `ReturnIntermediates | Timeout` an answer is one batch of records,
  CNAMEs first; a missing name and an empty answer are indistinguishable and both become NOERROR
  with no records (`spikes/netd-dns-probe/RESULTS.md`).
- **UDP** as a flow table onto connected host sockets, a minute's idle timeout, replies fragmented
  to the guest's MTU. The guest reaches routable addresses only: not the host's loopback,
  broadcast, multicast or its own subnet.
- **ICMP echo** through one unprivileged ICMP socket (`SOCK_DGRAM`, `IPPROTO_ICMP`). Every such
  socket receives every echo reply on the host, so each guest flow goes out under an identifier of
  netd's and only matching replies come back (`spikes/netd-icmp-probe/RESULTS.md`); pings of the
  gateway are answered on the link.
- **Port forwards** (the SSH forward among them): the worker binds each listening socket and hands
  netd a copy over a second socketpair (descriptor 4), and again to each netd that replaces one, so
  the port is the VM's for its whole life and netd never binds one. netd opens each accepted
  connection into the guest from the gateway's address. The supervisor moves a forward through the
  worker's network control link, and every worker spawned after (a reboot, reset or resume) binds
  the moved port.
- **IPv6** later.

**Enhanced tier.** A custom guest NIC driver writes frames straight into a guest-visible arena and
rings a doorbell page, as `virtio-gpu-limina` does for the Metal transport (an MMIO trap, ~1 µs):
netd reads the guest's frames where the guest wrote them and the worker's copies go away, while
netd sees only that arena, never guest RAM at large. Further out, socket-level offload (the guest's
`connect`/`send` forwarded to netd, skipping TCP in both guest and host; libkrun's TSI is prior art,
with its limits around listening and raw sockets). Both are additive: the stock guest's virtio-net
path is the floor.

**Multi-VM.** One netd per VM gives each fallen-back VM its own island. VMs that must see each other do so on a vmnet Network (§3.3); a VM that has fallen back
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
- **Bridged:** `VMNET_BRIDGED_MODE` with `vmnet_shared_interface_name_key`, chosen per VM in
  `vm.toml` (`mode = "bridged"`, `uplink = "en0"`) or the Configure sheet, whose Network popup
  lists `vmnet_copy_shared_interface_list` under System Settings' names. A bridged guest's traffic
  goes to the LAN without entering the host's routing, so a host VPN neither breaks it nor carries
  it; the sheet says so. Bridged is an explicit choice, never a fallback. It has no SSH forward:
  the LAN's DHCP server leases the address, so the worker reads the acknowledgement on its way to
  the guest (`krun::lease`, on every NIC) and reports it over the NIC's control link; the
  supervisor publishes it on its runtime socket (`address`), where the control center and
  `limina address` read it, and prints it to the VM log. A NIC that will not start is reported the
  same way, with the reason. Pre-flight refuses an uplink the host does not have, since vmnet's
  start never answers for one.

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
privacy (NECP): until the user answers its Local Network prompt every signature is dropped with
`EHOSTUNREACH`, and a binary signed with a real identity and the hardened runtime gets through once
allowed, from a shell and as a launchd job alike, while Apple's platform binaries (`/usr/bin/nc`,
`/usr/bin/ssh`) pass without asking (`spikes/vmnet-necp-probe/RESULTS.md`). A grant belongs to the
code identity that asked, so ad-hoc builds would need a click per build and a headless run none at
all. limina's own way in, the SSH forward, therefore never dials the guest's vmnet address (§3.5).

### 3.4 Uplink tracking

A shared network NATs out of one uplink, chosen when it is created (vmnet's default: the route-table
default; `vmnet_network_configuration_set_external_interface` to choose). No call changes a live
network's uplink. The worker therefore follows the host's routes (`crates/limina-vmm/src/krun/
uplink.rs`): a routing socket wakes it on any change, and once the burst settles it asks the kernel
(`RTM_GET`) for two routes — the way out (the route to an Internet address) and the route to vmnet's
subnet (to the address after the host's own on it, which vmnet reports at start; the host's own is a
local `/32` on `lo0` that says nothing). When the way out moves to another interface the worker
starts a fresh interface on it with the same MAC and moves the NIC onto it as on a switch (§3.5);
vmnet keys leases on the MAC, so the guest keeps its address (the spike measured 0.30–0.35 s per
rebuild). Open connections drop. When the routes say vmnet cannot serve (§4), the NIC moves to netd
instead.

### 3.5 Switching between vmnet and netd

**Automatic, in the worker, under one virtio device and one MAC** (`limina --net`;
`crates/limina-vmm/src/krun/switch.rs`). The NIC's backend is limina's switching backend, which
holds a running netd always and a vmnet interface while the NIC is on vmnet, and sends the guest's
frames to the side it is on. libkrun's net worker watches both sides' descriptors through its
backend's extra descriptors. Moves run on a controller thread of their own: vmnet's start and stop
each take up to seconds, which the net worker's thread never waits on, and a retired interface is
handed back to be stopped there.

- **To netd** when vmnet cannot serve: the interface does not start (privilege, `SERVICE_BUSY`, a
  start that does not answer within 10 s), vmnet's subnet is routed through something other than
  its bridge (a VPN client's route for it, §4), or the way out is a tunnel (under which forwarded
  UDP was lost, §4). A NIC whose way out is already a tunnel at boot starts on netd.
- **Back to vmnet** once the routes have stayed clear for 10 s, so a flapping VPN does not flap the
  guest; each start of vmnet that fails doubles the wait, up to 5 min. While on netd there is no
  bridge to compare against, so only a route of `/8` or narrower counts as a claim on the subnet:
  the default route and a VPN's split halves of it do not.
- **On request:** `limina net-switch <vm> vmnet|netd`, behind the `net-switch` harness lever (off by
  default), moves the NIC as a VPN would; a move to netd holds until a move back.

The guest has to notice that its subnet changed, so a move drops the carrier (virtio-net link
status: `VIRTIO_NET_F_STATUS` and a config-change interrupt, through libkrun's `NetLink`) and raises
it again after a second, and the guest's DHCP client asks the new side for a lease. Fedora 44's
NetworkManager does so whether the carrier was gone for 100 ms, 1 s or 5 s, and holds the new lease
1.5–1.9 s after the move began, HTTPS answering within half a second of it; the second leaves room
for clients that wait out a short flicker. Open connections drop, as on an uplink rebuild. Every
move is logged with its reason.

**The SSH forward is netd's on both sides.** On vmnet the guest's own frames go to vmnet, except
those for netd's subnet, which go to netd: netd carries each forwarded connection into the guest
from its gateway's address (`192.168.127.1`), the guest answers through its default route, and the
worker hands the answer to netd. The worker tells netd where the guest is (its vmnet address and
MAC, learnt from the guest's own frames) until the guest takes netd's lease. So `127.0.0.1:<port>`
works the same on either side, for every build, without the Local Network prompt a direct dial of
the guest's vmnet address needs (§3.3).

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

Measured 2026-10-19 on macOS 27.0.1 with a guest on the switching NIC and a Tailscale exit node: the exit
node took vmnet's `/24` with local network access on (through `utun8`) and with it off (through
`en0`); each time the NIC moved to netd 0.3 s after the route changed, the guest leased
`192.168.127.2` and reached HTTPS with the host's public address, and with the exit node off the
NIC came back to vmnet 10 s after the routes cleared, on its old address.

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
- **Phase 3 — `limina-netd` with gvproxy's feature set (done).** netd, libkrun's external-backend
  hook, limina-vmm's netd backend and `--net-netd`. The net tests (lease, outbound DNS, SSH forward
  and its runtime move, per-VM MAC, two VMs in parallel, pings) run on netd with its own log as the
  oracle, and `net_netd` adds netd's restart after a kill and the guest's own NTP and HTTP traffic.
- **Phase 4 — vmnet with the switch (done).** Uplink tracking (§3.4), the switch and link flap
  (§3.5), libkrun's link status and gathered descriptors, the SSH forward through netd on both
  sides. `net_auto` boots a guest on vmnet, moves it to netd through the lever (it
  leases and reaches the Internet, SSH throughout) and back, with no move it did not ask for; a
  vmnet that will not start leaves the VM on netd.
- **Phase 5 — vmnet with the switch by default; gvproxy retired (done).** `--net` is the switching
  NIC, for flat runs and managed VMs alike; gvproxy's supervisor half, its bundling, watchdog and
  reaper are gone. The net tests run on the default NIC (vmnet's lease for the guest's MAC, a name
  resolved in the guest, SSH through the forward, two VMs at once, a custom MAC) and on netd alone;
  a forward moved at runtime outlives a reset.
- **Phase 6 — `Network` model, per-VM MACs, vmnet network objects.** `limina net` CLI; one network
  object per Network with reservations; forwards per VM. *RED:* two VMs on two Networks, mutually
  invisible; two VMs on one Network ping each other.
- **Phase 7 — bridged as a product feature (done).** `vm.toml`'s bridged mode and uplink, the
  Configure sheet's Network popup, the worker learning the guest's address from its DHCP
  acknowledgement and the supervisor reporting it (`limina address`, the control center row),
  pre-flight's uplink checks, and vmnet's start failures named by status. `net_bridged` covers a
  flat and a managed VM bridged onto the host's Wi-Fi and the two ways an uplink can be missing;
  with `LIMINA_LAN_PEER` set, another machine on the LAN opens the guest's SSH port too (passed
  over Wi-Fi on both ends). Bridging over Ethernet is not yet exercised.
- **Phase 8 — enhanced-tier arena NIC.** The guest driver and netd reading the arena (§3.2).

## 7. Decision: vmnet shared by default, `limina-netd` as the fallback

Decided on Phase 1's measurements (§2): vmnet moves 2.3–2.5× the bytes at about a third of the host
CPU per byte, gives VMs real addresses the host reaches directly, and gives multi-VM networks
through network objects. Its weakness is §4, host VPNs, and that is what the automatic switch to
`limina-netd` covers. gvproxy goes: what it does, netd does with no socket hop, no Go runtime and no
large-MTU workaround, and netd's TCP is passt-style translation rather than a full stack.

## 8. Open questions

- **netd end to end:** the TCP spike measured netd's share on loopback alone; throughput through the
  worker, the ring and a guest, many connections at once, and UDP are not measured yet, nor what
  the offloads (§3.2) would buy.
- **The tunnel trigger:** UDP loss was measured under one client; switching on any tunnel uplink is
  the conservative rule, and other clients (WireGuard, Cisco, Zscaler) may need finer ones. Under
  Tailscale the subnet rule fired first both times, so the tunnel rule has not yet been seen to
  fire on its own.
- **An uplink change on vmnet:** the rebuild onto a new way out (Wi-Fi to Ethernet) is built but
  not yet exercised with the switch.
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
