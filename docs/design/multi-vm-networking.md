# Multi-VM & Bridged Networking for limina

Status: **proposal** · Host: macOS 26+ (Apple Silicon) · Audience: limina maintainer

Every networking feature here runs without root or a restricted Apple entitlement. Two backends
carry guest traffic: **gvproxy**, a user-mode NAT limina ships today, and Apple's
**`vmnet.framework`**, which a non-root process can drive in every mode with the unrestricted
`com.apple.security.virtualization` entitlement. **Which of them is the default NAT is an open
decision** (§7), to be settled by performance and efficiency measurements once vmnet runs through
a native libkrun backend (Phase 1).

Measured evidence lives in `spikes/vmnet-network-probe/RESULTS.md` (vmnet: privilege, leases,
network objects, VPN behaviour) and `spikes/net-bench/RESULTS.md` (gvproxy throughput and CPU).
Background: `docs/research/07-networking.md`. Code: `crates/limina/src/gateway.rs` (gvproxy
lifecycle), `crates/limina-vmm/src/krun/mod.rs` (`net_device`).

## 1. Goals

- **(a) Managed, isolatable networks.** Multiple concurrent VMs; VMs on the *same* limina network
  see each other; VMs on *different* networks are mutually invisible.
- **(b) Bridged-to-LAN.** A VM attached to a physical interface (Wi-Fi or Ethernet) takes a LAN
  lease and is a first-class peer.
- **(c) No orphaned helpers.** Nothing a VM spawns outlives it.
- **(d) Works behind the user's VPN.** A host VPN coming up, going down or changing must not leave
  a VM without the Internet.

## 2. What exists and what is measured

**gvproxy (shipped, `gateway.rs`).** One gvproxy per VM, listening on a vfkit-style unixgram socket
keyed on the supervisor pid; the worker attaches virtio-net to it (`UnixgramPath`, vfkit framing,
`NET_FLAG_CSUM_VALID`). DHCP leases the guest `192.168.127.2`; the host SSH forward auto-allocates
from 2222 up and moves at runtime through gvproxy's `forwarder/expose`/`unexpose` API; MTU 65520
(`GVPROXY_MTU`) and a raised `GOGC` are the measured throughput settings. A watchdog respawns a
gvproxy that dies under a running VM. Orphans are covered three ways: clean-exit teardown, an EOF
death-pact watcher (`limina __reap-gateway`), and a startup sweep of pid-files keyed on dead
supervisors. Goal (c) holds for gvproxy today.

Limits: a gvproxy serves exactly one vfkit peer (its listener `connect()`s to the first peer), so
VMs cannot share a segment — goal (a) is not met. gvproxy is structurally NAT-only — goal (b) is
out of reach. ICMP is limited and inbound LAN access absent through it. gvproxy traffic is the
host's own socket traffic, so a host VPN treats it like any app's.

Measured throughput (`spikes/net-bench/`, MTU 65520, M1 Max): guest → host 17.9 Gbit/s, host →
guest 8.0–8.7 Gbit/s, at ~130–175% worker CPU plus ~185–235% gvproxy CPU and 31–65 MB gvproxy
footprint mid-transfer (default `GOGC` vs 400).

**vmnet (spike only).** Measured on macOS 26.6.2, non-root, ad-hoc signed
(`spikes/vmnet-network-probe/`):

| | Result |
|---|---|
| Privilege | `com.apple.security.virtualization` unlocks every mode; `com.apple.security.hypervisor` alone (what `limina-vmm` carries) fails every mode. The two coexist on one binary. |
| Shared (Apple NAT) | DHCP lease in ~50 ms, router, DNS proxy, Internet |
| Host-only | Lease, host reachable, no router by design |
| Bridged over Wi-Fi `en0` | A real LAN lease from the LAN's DHCP server, Internet through the LAN router |
| virtio-header + TSO mode | Starts; max packet 65550; every frame carries a 12-byte `virtio_net_hdr` |
| Network-object API (macOS 26) | Own guest MAC; DHCP reservations honoured; two interfaces on one network see each other; a second network is isolated at L2 and L3; NAT works |
| Port forwards | Delivered for traffic arriving on a real interface (LAN, tailnet), not for host-local connects |
| Host → guest | The host reaches a guest's address on the bridge directly; no forward needed |

Not measured: throughput and CPU through a real guest, macOS 27, a Developer ID + notarized build,
the App Store sandbox, sleep/wake, Wi-Fi roaming, Internet Sharing being on
(`VMNET_SHARING_SERVICE_BUSY`), and pf state left behind after teardown (apple/container #2335
reports a `scrub … no-df` rule breaking host IPv4 on some ISPs).

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
backend fd and MAC. The per-VM MAC already exists: `vm.toml` stores one derived from the VM uuid
(`vmlib/schema.rs` `mac_for_uuid`), and gvproxy's config mode binds its static lease to it.

A user-mode NAT NIC is a `SOCK_DGRAM` socketpair, one end to libkrun as `UnixgramFd`
(`new_unixgram_fd`, vfkit framing on), the other to gvproxy; today's NIC uses `UnixgramPath`, and
moving it to a supervisor-created socketpair is Phase 0. A vmnet NIC uses libkrun's native vmnet
backend (§3.3) and has no socket at all.

### 3.2 User-mode NAT

gvproxy as shipped (§2), one per VM. Multi-VM on one user-mode segment (goal (a) without vmnet)
needs a multi-peer switch: either patch gvproxy's vfkit listener to demux peers (its internal L2
switch is already multi-endpoint), or own the switch in a refcounted user daemon,
**`limina-networkd`**, that terminates each VM's socketpair onto a per-Network segment. How much of
this to build depends on §7: if gvproxy remains the default it is the main multi-VM path; if vmnet
becomes the default, vmnet network objects deliver goal (a) (§3.3) and user-mode NAT needs only
per-VM isolation as a fallback.

### 3.3 vmnet in the worker

**The worker (`limina-vmm`) holds the vmnet interface, through a native vmnet backend in libkrun's
virtio-net.** The worker gains `com.apple.security.virtualization` beside
`com.apple.security.hypervisor`; the interface lives in the worker process and dies with it, so
there is no helper and nothing to orphan. Public API only: batched `vmnet_read`/`vmnet_write`
(`vmnet_read_max_packets_key`, macOS 15+), no private syscalls, no vendored C.

The backend starts the interface with `vmnet_enable_virtio_header_key` + `vmnet_enable_tso_key`,
so the `virtio_net_hdr` passes straight between the guest's virtqueues and vmnet in both directions:
64 KiB TSO frames and checksum offload end to end, and no socket hop (on the gvproxy path that hop
costs ~0.6 of ~3.5 cores at 8 Gbit/s, `spikes/net-bench/`). Mechanism in libkrun, on the fork's
`limina` branch and upstreamable: the backend takes a mode, a bridged interface name or a
network object, and the guest MAC. Policy in limina: which Network, which mode, uplink tracking
(§3.4), VPN detection (§4).

A relay over libkrun's existing unixgram backend would need no libkrun change, but that backend
writes a blank `virtio_net_hdr` on receive and strips it on transmit (`unixgram.rs`
`read_frame`/`write_frame`), so the guest's segmentation and checksum metadata could never reach
vmnet; the native backend is the one that can use what vmnet offers.

**Modes onto Networks:**

- **NAT (shared) and Host:** one vmnet network per limina Network, created with the macOS 26
  network-object API and handed to each attached worker (`vmnet_network_copy_serialization` →
  `vmnet_network_create_with_serialization`). DHCP reservations
  (`vmnet_network_configuration_add_dhcp_reservation`) give each VM's MAC a stable address; a
  separate network per Network gives isolation. Cross-process serialization is not yet measured.
- **Bridged:** `VMNET_BRIDGED_MODE` with `vmnet_shared_interface_name_key`; the UI lists
  `vmnet_copy_shared_interface_list`. A bridged guest kept the Internet under a Tailscale exit
  that took shared mode down, presumably because its traffic goes to the LAN without entering the
  host's routing — which would also mean it never uses the tunnel (egress path not checked); the
  UI should say so.

**API facts the code must respect:**

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

**Host → guest.** Any limina process that dials a guest's vmnet address (an SSH readiness probe,
the UI) is subject to NECP: our ad-hoc binaries, with or without entitlements, are dropped with
`EHOSTUNREACH` while Apple's `/usr/bin/nc` gets through — consistent with Local Network privacy.
Whether a Developer ID app passes after the user grants Local Network access is unmeasured.

### 3.4 Uplink tracking

A shared network NATs out of one uplink, chosen when it is created (vmnet's default: the route-table
default; `vmnet_network_configuration_set_external_interface` to choose). No call changes a live
network's uplink. The worker therefore watches the host's route (routing-socket `RTM_GET`, or
SystemConfiguration's `State:/Network/Global/IPv4` `PrimaryInterface` — the two agreed on every
change the spike saw; it polled both once a second, while the product would subscribe to change
notifications, which is untested) and on a change stops the interface, releases the network,
and rebuilds both on the new uplink with the same MAC. Measured: 0.30–0.35 s, same guest address,
next-check detection. The guest sees a link reset; open connections drop. A rebuild onto a VPN's
uplink restores the Internet only as far as §4 allows: under a Tailscale exit node it does not.

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

**Consequences, whichever backend is the default:**

- **Detect and say.** The worker checks that its subnet's route points at its own bridge; when a
  VPN has taken it, it reports that plainly in the UI instead of a silently dead network.
- **Fallback.** A VM on a vmnet NAT network that loses the Internet this way can move to user-mode
  NAT (a NIC swap the guest sees as a link change). Whether that is automatic is part of §7.
- **Never gate the floor.** If vmnet cannot start (privilege, `SERVICE_BUSY`, a hung create), the VM
  comes up on user-mode NAT.

## 5. Two-tier mapping

The tiers are about the *guest*, and both backends serve a stock guest: an unmodified Fedora runs
NetworkManager DHCP against gvproxy or vmnet alike, with no limina components. The constraints are
host-side: user-mode NAT is the backend that always works (no entitlement, immune to VPNs), so it
is what the VM falls back to when vmnet cannot serve. `limina-agent` adds niceties (reporting the
guest's address, hostnames) and is never required.

## 6. Phases (RED-first, bisectable)

Each phase ships on its own and is tested against the shipped binaries (`crates/limina-test`).

- **Phase 0 — fd-backend migration.** The existing gvproxy NIC moves from `UnixgramPath` to a
  supervisor-created socketpair (`UnixgramFd`, vfkit on). *RED:* the stock guest still DHCPs and
  SSHes.
- **Phase 1 — native vmnet backend, then the measurement.** The libkrun vmnet backend of §3.3 on
  the fork's `limina` branch; worker entitlement; `--net-vmnet shared|bridged` (experimental).
  *RED:* a stock guest leases and reaches the Internet in shared and bridged mode. Once it works,
  run `spikes/net-bench/netbench.sh` against the same guest on both backends: iperf and ssh both
  directions, worker CPU, kernel time, gvproxy CPU and footprint, idle cost, small-packet latency.
  Bridged too. **Closes §7.**
- **Phase 2 — `Network` model + per-VM MACs + dynamic forwards.** `limina net` CLI; per-VM MAC to
  the worker; forwards per VM. *RED:* two VMs on two Networks, distinct SSH ports, mutually
  invisible.
- **Phase 3 — vmnet NAT/Host as a product feature.** Network objects with reservations, uplink
  tracking (§3.4), VPN detection and fallback (§4), host → guest reachability with Local Network
  permission. *RED:* the host reaches a VM by its
  address; a VPN taking the subnet route is reported; a failed vmnet start leaves the VM on
  user-mode NAT.
- **Phase 4 — bridged.** Interface picker, `NOT_AUTHORIZED`/`SERVICE_BUSY` surfaced. *RED:* the
  guest takes a LAN lease and is reachable from another LAN host over Wi-Fi and Ethernet.
- **Phase 5 — vmnet Networks spanning VMs.** One network object shared across workers by
  serialization. *RED:* two VMs on one Network ping each other; two Networks stay isolated.
- **Phase 6 — multi-VM on user-mode NAT (`limina-networkd`).** Scope set by §7: the main multi-VM
  path if gvproxy is the default, a reduced fallback otherwise. Tailscale Option A (§9) needs
  the multi-peer user-mode segment either way, so wanting Option A keeps that part in scope.

## 7. Open decision: the default NAT backend

**User-mode NAT (gvproxy) or vmnet shared mode — open pending Phase 1.** What each side has today:

- **For vmnet:** real VM addresses the host reaches directly, multi-VM networks and isolation from
  Apple's network objects, ICMP and arbitrary protocols, stable addresses by reservation, no extra
  process per VM, a kernel data path and a native virtio-header/TSO path in reach.
- **For gvproxy:** immune to host VPNs by construction (§4 — vmnet shared mode loses the Internet
  or UDP under a common Tailscale configuration, and other clients fail in other ways), needs no
  entitlement or Local Network grant, and leaves no host network state behind.

What Phase 1 must measure to decide: throughput both directions (iperf and ssh) against the
net-bench gvproxy table; total CPU per Gbit/s including kernel time and gvproxy; memory (gvproxy's
footprint vs vmnet's kernel side); idle cost with a quiet guest; latency. The VPN findings stand
either way: whichever is the default, the other is the fallback, and §4's detection is required
whenever a VM is on vmnet NAT.

## 8. Open questions

- **Cross-process network objects:** `vmnet_network_copy_serialization` across workers is the basis
  of Phase 5 and is not yet exercised.
- **Forwarded UDP under an exit node:** where it is lost (pf NAT on `utun`, or the VPN client) is
  unknown without root.
- **Distribution:** whether a Developer ID + notarized, or App Store, build gets the same vmnet
  behaviour (`docs/design/distribution.md`); re-run `spikes/vmnet-network-probe` in each.
- **macOS 27:** the network-object API, and the reported indefinite `vmnet_network_create`.
- **Coexistence:** Apple `container` and other vmnet users on the same host; subnet collisions with
  the LAN; pf state after teardown.
- **Wi-Fi roaming and sleep/wake** on both backends (gvproxy has its own open Wi-Fi-roam bug,
  gvisor-tap-vsock #648).

## 9. Tailscale integration (fleet-wide, no per-VM config)

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
| **A** | Embedded node in `limina-networkd` (`ProcessSubnets` + patched dial into the gvproxy L2 segment) | **None** | **Yes** | No (subnet-router) | High (vendored TS patch) | **None** | **Strategic target** (after Phase 6 `limina-networkd`) |
| **C** | Agent-injected per-VM `tailscaled` (ephemeral *tagged* auth key over vsock) | None (agent injects) | No (needs agent) | **Yes** | Medium | None on host; guest-internal TUN only | **Near-term win** |
| **B** | Host-side `tailscaled` subnet router over a vmnet `bridge100` (`--advertise-routes=<CIDR>`) | None | Yes (on a vmnet Network) | No | Low | vmnet (`com.apple.security.virtualization`) | **After Phase 3** (vmnet tier) |

- **Why A reaches stock guests and B can't:** the default gvproxy `/24` exists only inside gvproxy's
  userspace netstack — there's nothing host-routable for an external router to advertise. A (inside
  the daemon that owns that netstack) is the only path that brings the tailnet to the **zero-privilege
  NAT floor a stock guest gets by default**, satisfying the two-tier guarantee. B works only where
  vmnet has produced a real bridge interface — so it follows Phase 3.
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
   own-the-stack tenet exists for. Depends on Phase 6 (`limina-networkd`)
   landing first, with its multi-peer segment, whatever §7 decides.
3. **After Phase 3 — Option B** (host `tailscaled` over vmnet). Cheap once VMs sit on a vmnet
   Network; it covers only VMs attached to one. Measure it against §4 first: the host `tailscaled`
   is the same client whose local-access exclusion takes vmnet subnets away from their bridge
   under an exit node, and whether advertising a vmnet subnet trips the same logic is unmeasured.

**Rollout addendum (extends §6, unprivileged-first):**
- **Phase 7 — Tailscale opt-in, Option C:** `--tailscale` on a Network; `limina-agent` brings up
  `tailscaled` in agent-bearing guests via an ephemeral tagged key over vsock; route/identity via
  per-VM node. *RED:* an agent guest on a `--tailscale` Network is reachable by its MagicDNS name from
  another tailnet peer; a stock guest on the same Network is unaffected.
- **Phase 8 — Tailscale Option A (embedded subnet router):** after `limina-networkd`; vendor the
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
