# Multi-VM & Bridged Networking for limina

Status: **proposal** · Target host: macOS 26.x (Apple Silicon) · Audience: limina maintainer

> Provenance: synthesized from a 5-track research sweep (vmnet.framework, socket_vmnet vs
> vmnet-helper, gvproxy/gvisor-tap-vsock, peer projects lima/colima/podman-machine/UTM/vfkit,
> macOS helper-lifecycle), each adversarially fact-checked. The two load-bearing *in-tree*
> claims were verified directly: libkrun ships `VirtioNetBackend::UnixgramFd(RawFd)`
> (`third_party/libkrun/src/devices/src/virtio/net/device.rs:66`, wired through `worker.rs:58`)
> and an in-tree vmnet-helper integration test
> (`third_party/libkrun/tests/test_cases/src/test_net/vmnet_helper.rs`). So "essentially no
> libkrun patches" is grounded, not aspirational. Companion: `docs/research/07-networking.md`
> (M3 NAT background), `crates/limina/src/gateway.rs`, `crates/limina-vmm/src/krun/mod.rs`.

> **Scope: unprivileged only.** Every networking feature limina ships runs without root or a
> restricted Apple entitlement. That includes vmnet: on macOS 26 a non-root process with the
> unrestricted `com.apple.security.virtualization` entitlement runs vmnet SHARED, HOST and BRIDGED
> (`spikes/vmnet-network-probe/RESULTS.md`), so §3.3 and Phases 4–6 need no privileged helper and
> are no longer a client of `limina-privhelperd` (`docs/design/privileged-helper.md`). Order of work:
> the gvproxy → `limina-networkd` NAT path, the `Network` abstraction, multi-VM + isolation, dynamic
> per-VM forwards and the orphan fix first (§3.1–3.2, §4; Phases 0–3), then vmnet (§3.3; Phases 4–6)
> as the opt-in enhanced tier.

## 1. Problem & goals

limina's networking today is a single user-mode NAT path that works well but is structurally
single-VM. The supervisor spawns one `gvproxy` (gvisor-tap-vsock) per VM listening on a
vfkit-style unixgram socket (`crates/limina/src/gateway.rs` `start` → `spawn_gvproxy`), and the
worker attaches one virtio-net device whose backend is `UnixgramPath(socket, vfkit=true)` with the
well-known vfkit MAC `5a:94:ef:e4:0c:ee` (`crates/limina-vmm/src/krun/mod.rs:63`, `add_net` at
`:304`). gvproxy's default config statically leases that MAC `192.168.127.2` and pre-forwards host
`127.0.0.1:2222 → guest:22`.

Three goals drive this work:

- **(a) Managed, isolatable NAT bridges.** Run multiple concurrent VMs; let VMs on the *same*
  limina network see each other; keep VMs on *different* networks mutually invisible.
- **(b) Bridged-to-LAN.** Attach a VM directly to a Mac physical interface (`en0` Wi-Fi /
  Ethernet / Thunderbolt) so it gets a DHCP lease from the LAN and is a first-class peer.
- **(c) No orphaned helpers.** A NAT/bridge helper must never outlive the VM(s) it serves.

Two current limitations block (a) and (c):

- **L1 — effectively single-VM.** A `gvproxy` instance serves exactly one vfkit peer by
  construction: its unixgram listener `AcceptVfkit`s once and `connect()`s the datagram socket to
  the first peer's address; `connectedUnixgramConn.Write` always `WriteTo` that single remote
  (gvproxy `pkg/transport/unixgram_unix.go`, `cmd/gvproxy/main.go`; corroborated empirically in
  containers/gvisor-tap-vsock#383, where a second VM never gets an IP). A second limina VM would
  also reuse the same MAC/`.2` lease and its `gvproxy` would want to bind host `127.0.0.1:2222`
  again → collision. Note `krun_set_port_map` is **TSI-only and returns `EINVAL` once a virtio-net
  device exists**, so per-VM forwards cannot use it.
- **L2 — orphans.** `gvproxy` is spawned in its **own process group** (`cmd.process_group(0)` in
  `spawn_gvproxy`, so a terminal Ctrl-C to the foreground group doesn't kill it early). Cleanup is
  best-effort: `Gateway`'s `Drop` (headless) or the idempotent `cleanup()` SIGTERM→500ms→SIGKILL
  ladder (windowed `process::exit`). **Neither runs on a hard SIGKILL or panic of the supervisor**
  — macOS has no `PR_SET_PDEATHSIG`, so the own-group child is simply orphaned and reparented to
  launchd.

The core insight from the research: gvproxy's *internal* L2 switch is genuinely multi-endpoint
(`pkg/tap/switch.go` keeps a `conns` map + a MAC→connID CAM table, floods broadcasts, unicasts by
MAC) — only the **vfkit listener wiring** is single-peer. And bridged-to-LAN is structurally
impossible for gvproxy (strict user-mode NAT, no physical-interface code) — that capability lives
only in Apple's `vmnet.framework`.

## 2. The design space

The realistic host-side backends, scored against limina's needs. (passt/pasta is omitted from the
table: it is **Linux-host-only** — Darwin is an unimplemented wishlist item, passt.top — so it
cannot run on a macOS host at all.)

| Backend | Privilege (macOS 26) | Multi-VM on one segment | Bridged-to-LAN | Fits two-tier? |
|---|---|---|---|---|
| **gvproxy** user-mode NAT | none / none | No (one peer per instance; switch is multi-endpoint but the vfkit listener isn't) | **No** (structural) | **Yes — this is the floor** |
| **passt/pasta** | n/a (does not run on macOS) | n/a | n/a | n/a |
| **vmnet SHARED** | `com.apple.security.virtualization`\* | Yes (same vmnet network = shared L2 via `bridge100`) | No (NAT only) | Yes (enhanced) |
| **vmnet BRIDGED** | `com.apple.security.virtualization`\* | Yes (whole LAN) | **Yes** | Yes (enhanced) |
| **vmnet HOST** | `com.apple.security.virtualization`\* | Yes (isolated L2, no Internet) | No (by design) | Yes (enhanced) |

\* Measured on macOS 26.6.2, M1 Max, non-root, ad-hoc signed (`spikes/vmnet-network-probe/`):
with this unrestricted entitlement all three modes move packets through the classic
`vmnet_start_interface` API; with only `com.apple.security.hypervisor`, or none, all three fail.
The restricted `com.apple.vm.networking` plays no part. Developer ID + notarization, the App Store
sandbox and macOS 27 are unmeasured.

One design rule that shapes the table:

- **Isolation must come from separate vmnet networks/instances, not merely different subnet
  numbers.** The header guarantees intra-network reachability by default but does *not* promise that
  a different `/24` alone severs L2 reachability. And do **not** use `vmnet_enable_isolation_key` for
  the shared-bridge goal — it does the *opposite* (severs intra-bridge comms).

## 3. Proposed architecture

### 3.1 A first-class `Network` abstraction

Introduce a named **Network** object that VMs attach to — the same shape lima/podman converged on.
This replaces the implicit "one gvproxy, hard-coded MAC/.2/2222" model.

```
Network {
    name:    String,              // stable id, e.g. "default", "lab-a"
    type:    Nat | Bridged | Host,
    // NAT/Host only:
    subnet:  Option<Ipv4Net>,     // omit → auto-pick a free /24
    // Bridged only:
    uplink:  Option<String>,      // physical NIC, e.g. "en0"
}

NicAttachment {                   // one per VM virtio-net device
    network: NetworkRef,          // which Network this NIC joins
    mac:     MacAddr,             // per-VM stable MAC (NOT the shared 5a:94 well-known one)
}
```

A VM references one or more Networks; each reference becomes one virtio-net device. **Multiple
NICs per VM** map directly onto libkrun's multi-NIC C API — `eth0`, `eth1`, … each with its own
backend fd and MAC. The data-plane wiring is uniform across all network types: **the supervisor
creates a `SOCK_DGRAM` socketpair per NIC, hands one fd to the per-NIC helper and the other to
libkrun via the fd variant** (`krun_add_net_unixgram(fd, mac, features, flags)` — libkrun's
`UnixgramFd(fd)` branch; passing fd≥0 *and* a path is `EINVAL`). The libkrun side is **already
there** and already exercised: `third_party/libkrun/tests/test_cases/src/test_net/vmnet_helper.rs`
creates exactly this `AF_UNIX`/`SOCK_DGRAM` socketpair, passes `--fd 3` to vmnet-helper, reads its
JSON (MAC/subnet) from stdout, and feeds the local fd into the VM. So the migration from today's
`UnixgramPath(path, vfkit)` to `UnixgramFd(fd, flags)` is a small change to `NetSpec`/`add_net`,
not new libkrun mechanism.

**`NET_FLAG_VFKIT` differs per backend** (a load-bearing detail): gvproxy expects the 4-byte
`VFKT` magic on connect (flag **on**, as today); vmnet-helper does **not** speak the vfkit dialect
— it recognizes and *drops* the `VFKT` frame as invalid, so for vmnet-helper the flag is **off**
and frames are raw L2, one per datagram. `NetSpec` therefore carries the flag per NIC, set by the
Network type.

### 3.2 NAT with multiple isolated bridges — **decision: one helper per Network, not per VM**

This is the central architectural call. Two viable shapes:

- **One-gvproxy-per-VM** (today, generalized): cheap, zero gvproxy patches, each VM is its own NAT
  island. But it **cannot** deliver the "same-network VMs see each other" half of goal (a) —
  separate gvproxies have no shared L2.
- **One helper per Network** multiplexing all its VMs on one subnet: delivers *both* halves of (a)
  — intra-network visibility *and* cross-network isolation — with far fewer host processes (one
  per network, not per VM).

**Recommendation: one NAT helper per Network.** This is what the goal actually demands. For the NAT
case I recommend hosting it under a single long-lived **`limina-networkd`** user daemon (see §4)
that owns all NAT Networks and gives each VM a socketpair fd onto the right network's switch. There
are two ways to get a multi-peer NAT switch under that daemon, and I pick the second:

1. *Patch gvproxy's vfkit listener to demux multiple peers* — track each guest's reply address,
   route by CAM→peer-addr instead of one `connect()`ed remote. gvproxy's switch is already
   multi-endpoint, so the change is localized to `unixgram_unix.go` + the single-`Accept` in
   `main.go`. This is upstreamable ("mechanism in the dep, policy in limina") but is real Go work
   in a vendored-and-rebuilt dependency.
2. **Own the NAT switch in `limina-networkd` directly.** Since we own the stack and already need a
   daemon for lifecycle and multi-network policy, `limina-networkd` *is* the multi-endpoint switch:
   it terminates N per-VM socketpair fds onto a per-Network L2 segment and runs a gVisor-style
   user-mode netstack (or supervises one gvproxy per network and demuxes in front of it). limina
   assigns per-VM MACs and per-VM DHCP leases (stable IPs keyed on MAC), and isolation is simply
   "different Network = different segment."

I pick (2) as the target and (1) as an acceptable interim if we want to ship faster on stock
gvproxy. Either way:

**Per-VM SSH/port forwards — decision: dynamic, via the gvproxy REST forwarder API, never the
static 2222.** Drop the hard-coded `127.0.0.1:2222` and the static `.2` lease. On VM-ready, `POST
/services/forwarder/expose {local:"127.0.0.1:<allocated>", remote:"<guest-ip>:22",
protocol:"tcp"}` over the network's `-listen` unix socket; `unexpose` on VM-stop so host ports
release deterministically. This is the documented lima/podman pattern and **sidesteps the
`krun_set_port_map` TSI/EINVAL limitation entirely** (it's host-side, not a krun port map). If
`limina-networkd` owns the netstack itself, it exposes the same expose/unexpose verbs on its own
control socket. Guest IP per VM comes from a deterministic DHCP reservation keyed on the per-VM
MAC, or is read back from the helper/agent.

### 3.3 vmnet: bridged-to-physical and Apple NAT — **decision: the worker holds the vmnet interface; no helper**

Bridged (and the vmnet SHARED/HOST modes) go through Apple `vmnet.framework`. It needs only the
unrestricted `com.apple.security.virtualization` entitlement (§2), which ad-hoc and Developer ID
signatures can carry, so there is no privilege left to isolate in a separate helper. **The worker
(`limina-vmm`) opens the vmnet interface itself and relays frames over an in-process `SOCK_DGRAM`
socketpair** whose other end goes to libkrun as `UnixgramFd` (vfkit off, one raw L2 frame per
datagram) — the job vmnet-helper does, done in the process that already owns the VM. The worker is
already launchd-started and entitled for HVF; it gains `com.apple.security.virtualization` beside
`com.apple.security.hypervisor` (both coexist on one binary, measured).

Why in-worker rather than a vmnet-helper child:

1. **Nothing to separate.** A helper existed to carry root or the restricted entitlement away from
   the VMM; neither is needed.
2. **Lifecycle for free.** The vmnet interface dies with the worker, so §4's orphan problem does not
   arise for vmnet at all; no death-pact, no sweep, no extra process per VM.
3. **No vendored C, no private syscalls.** vmnet-helper's throughput path uses private
   `sendmsg_x`/`recvmsg_x`; our relay calls only the public `vmnet_read`/`vmnet_write`.
4. **libkrun unchanged.** The `UnixgramFd` transport is the same one Phase 0 introduces for gvproxy.

**Still open — where the relay lives:** the in-worker relay above (recommended: no libkrun change,
reuses the unixgram path) vs. **a native vmnet backend in libkrun's virtio-net** (mechanism in
libkrun, removes the socketpair copy). Decide on throughput: measure the relay first, and write the
libkrun backend only if the copy shows up.

Mapping to Networks:
- **NAT-with-real-IP / Host groups:** SHARED (or HOST) per Network; all VMs of a group share the
  subnet → mutual visibility via `bridge100`; separate Networks = separate vmnet networks =
  isolation. Each worker is its own process, so a group spanning several workers needs either the
  classic API's network-identifier/subnet keys to land them on one network, or the macOS 26
  network object created once (by the supervisor or `limina-networkd`) and handed to each worker
  with `vmnet_network_copy_serialization` / `_create_with_serialization`. **Neither is verified:**
  the probe moved packets only through the classic API, and the network-object interface started
  but reported no gateway address. (For NAT the default stays the gvproxy floor; vmnet SHARED is the
  *enhanced* "reachable VM IP" upgrade.)
- **Bridged (goal b):** `VMNET_BRIDGED_MODE` + `vmnet_shared_interface_name_key = en0`; enumerate
  eligible NICs with `vmnet_copy_shared_interface_list` for the UX. Guest gets a LAN DHCP lease.
  Surface `VMNET_NOT_AUTHORIZED` (1010) and `VMNET_SHARING_SERVICE_BUSY` (1009, e.g. macOS Internet
  Sharing conflict) as clear user errors.
- **Port forwards in SHARED mode:** `vmnet_interface_add_port_forwarding_rule` (`vmnet.h:683`)
  replaces gvproxy's REST `expose` for SSH and user forwards — not yet exercised.
- **Stable guest MAC:** pin per VM (the start parameters' interface id / our per-VM MAC); stop
  relying on the gvproxy well-known MAC and static `.2`. SSH reaches the vmnet-assigned IP (read back
  from the interface parameters / the limina-agent / ARP on the bridge), or a port-forward rule.

**Gating.** vmnet is opt-in. If starting the interface fails, the VM comes up on NAT (or with no
NIC) with a clear error — **none of this is on the boot path of a stock NAT VM.**

### 3.4 Two-tier mapping

- **Baseline (stock guest, zero privilege, always works — the default):** gvproxy/`limina-networkd`
  user-mode NAT. A stock unmodified Fedora guest just runs NetworkManager DHCP against the built-in
  DHCP/DNS/NAT — no root, no entitlement, no guest components, no limina-agent. This is the floor
  every richer feature degrades to, and **networking can never make a stock guest fail to boot**: if
  a vmnet interface cannot start, the VM falls back to NAT (or comes up with no NIC) rather
  than failing.
- **Enhanced / opt-in:** vmnet SHARED/HOST (reachable VM IPs, intra-group visibility), vmnet BRIDGED
  (LAN-peer), and on macOS 26 the `vmnet_network_*` network-object API (§5). These layer
  **additively, per feature** — each comes up or falls back on its own; a VM can
  have NAT now and bridged later. The limina-agent is *not* required for any networking baseline; it
  only adds niceties (reporting the guest's leased IP back to the host for SSH targeting, per-VM
  hostname injection, etc.).

## 4. Helper lifecycle redesign (fixing the orphan bug)

macOS has no `PR_SET_PDEATHSIG`. The robust fix layers three mechanisms.

**(1) Death-watch — EOF on an inherited socket (the death-pact).** Keep the helper in its own
process group (so terminal Ctrl-C can't kill it early) **but** add an EOF-on-inherited-fd watch so
abnormal supervisor death still tears it down. The data-plane `SOCK_DGRAM` socketpair we *already
need* is the watch fd: when the supervisor (and its libkrun worker) die, the kernel closes their
fds, the helper's read side sees `EV_EOF`, and it exits. This is **robust to SIGKILL/panic with
zero cooperation from the dying parent** and has no pid-reuse hazard — strictly better than `kqueue
EVFILT_PROC|NOTE_EXIT` on the parent pid (which must handle `ESRCH`-means-already-dead and is
pid-reuse-prone; keep it only as optional belt-and-suspenders). For gvproxy specifically, verify it
exits when its vfkit datagram peer disconnects; if not, wrap it in a tiny limina-owned watcher holding the inherited fd that `killpg`s gvproxy on EOF (preferred
over patching the Go).

**(2) Startup orphan-sweep (backstop).** On every limina launch, scan a limina-owned registry dir
of pidfiles **keyed by stable Network id** (not the volatile supervisor pid the socket name uses
today at `gateway.rs`), and `SIGTERM→SIGKILL` any leftover helper whose owning Network/VM is gone —
with **identity verification** (exe path via `proc_pidpath` / start-time / a limina-written cookie)
to dodge pid-reuse. This catches residue that beats both the kill-ladder and the death-watch (power
loss, double-fault).

**(3) Orderly ladder (unchanged).** Keep the existing `SIGTERM→grace→SIGKILL`+reap for normal exits.

**Key decision: a single `limina-networkd` daemon for NAT; vmnet needs no helper at all.**

- **NAT → one long-lived user `limina-networkd`.** It owns all NAT Networks (multiple isolated
  bridges with distinct subnets/MACs), **refcounts** attached VMs, idle-exits at zero, and is the
  single thing to reap. This is what unlocks multi-VM + isolation cleanly and centralizes lifecycle.
  `limina-networkd` is itself supervised by the limina app via the same EOF death-watch + startup
  sweep, so centralizing does not reintroduce orphans. (If it hosts child gvproxy processes, those
  get the nested death-pact too.)
- **vmnet (shared/host/bridged) → inside the worker** (§3.3). The interface's lifetime is the
  worker's, so there is no helper to orphan and nothing for the death-watch or sweep to cover.

## 5. What we patch vs. reuse

- **libkrun — essentially no patches.** The multi-NIC fd API already exists and is exercised
  (`krun_add_net_unixgram`/`unixstream` with `UnixgramFd(fd)`; the vmnet-helper `--fd 3` test
  in-tree). limina's change is host-side: extend `NetSpec` to carry an fd + the per-NIC vfkit flag,
  switch `add_net` from `UnixgramPath` to `UnixgramFd`, and allow N NICs; for vmnet the worker owns
  both ends of the socketpair (§3.3). "Mechanism already in libkrun, policy in limina" holds
  verbatim. A native vmnet backend in libkrun's virtio-net is the one patch on the table, and only
  if the relay copy measures as a cost (§3.3). (The one *possible* libkrun-adjacent patch is the
  gvproxy vfkit-demux of §3.2 option 1 — but the recommended path owns the NAT switch in
  `limina-networkd` and avoids even that.)
- **vmnet-helper — not adopted.** Its reasons to exist (holding root or the restricted entitlement
  away from the VMM) do not apply (§2), and the in-worker relay does its job with public API only.
  It stays the reference for framing (raw L2 per datagram, vfkit off) and for the in-tree libkrun
  test that exercises the same `UnixgramFd` path.
- **socket_vmnet — do not adopt** (stream framing, a shared root daemon that floods frames to every
  client — lima-vm/socket_vmnet#58 — and no need for root at all).
- **gvproxy — reuse as-is for the floor**, or absorb its role into `limina-networkd`. No fork
  required for the recommended path.
- **macOS 26 `vmnet_network_*` network-object API — the multi-worker Network mechanism, unproven.**
  Terminology, to be precise: there is **no Apple facility called "vmnet-broker."** The genuine
  macOS 26 facility is the **`vmnet_network_*` network-object API** inside `vmnet.framework`
  (`vmnet_network_configuration_create` / `vmnet_network_create` — which *reserves* a subnet so
  per-VM interface starts can't collide — `vmnet_network_copy_serialization` /
  `_create_with_serialization` to pass the network to another process as an XPC object,
  `vmnet_network_configuration_add_port_forwarding_rule`, and `vmnet_interface_start_with_network`;
  verified: zero occurrences of "broker" in `MacOSX26.5.sdk/.../vmnet.h`). **`vmnet-broker` is a
  *separate third-party project*** (`github.com/nirs/vmnet-broker`, same author as vmnet-helper) — a
  userspace daemon that *consumes* that Apple API and brokers a network across processes over XPC.
  The named-network model maps onto "one Network per isolation group": the supervisor or
  `limina-networkd` creates the network, serializes it, and each worker starts its interface on it.
  The probe started an interface this way under the same entitlement, but the start reported no
  gateway address and no traffic was attempted, so **this path must be proven (packets, isolation
  between two networks, pre-set port forwards) before Phase 6 depends on it.** Call it directly; do
  not depend on `nirs/vmnet-broker`.
- **Guest agent (limina-agent).** Optional and additive only: report the guest's DHCP-leased IP back
  to the host (so SSH/forwards target the right address without ARP scraping), inject per-VM
  hostnames, surface link state. Never required for any networking tier.
- **CLI / UX surface.**
  - `limina net create <name> --nat [--subnet CIDR]` · `--host` · `--bridged en0`
  - `limina net ls` / `limina net rm <name>` (mirrors `limactl network create/list/delete`)
  - `limina --network <name>` to attach a VM (repeatable for multiple NICs); `--network <name>:mac=…`
    to pin a MAC.
  - `--bridged en0` as sugar for "create+attach an ephemeral bridged Network on `en0`."
  - Default (no flag) = attach to the built-in `default` NAT Network — preserving today's "just
    works" experience.

## 6. Phased rollout (RED-first, bisectable)

Each phase ships independently and is testable against the shipped binaries (`limina` →
`limina-vmm`) via `crates/limina-test`; full validation via `scripts/test-boot.sh`.

**Order:** Phases 0–3 (NAT, multi-VM, orphans) first; Phases 4–6 (vmnet) after, as the opt-in
enhanced tier. No phase needs root or a restricted entitlement.

- **Phase 0 — fd-backend migration (no behavior change).** Switch the existing single gvproxy NIC
  from `UnixgramPath` to a supervisor-created socketpair fd (`UnixgramFd`, vfkit on). *RED:* a test
  asserting the stock guest still DHCPs and SSHes. Lands the fd plumbing with zero feature risk.
  (M3 hardening.)
- **Phase 1 — orphan fix.** Add the EOF death-watch on the data socketpair + the stable-keyed
  startup sweep; keep the orderly ladder. *RED:* spawn a VM, `SIGKILL -9` the supervisor, assert no
  surviving gvproxy/helper after a bounded wait; assert a stale helper from a prior crash is swept
  on next launch. Closes L2, backend-agnostic. (M3.)
- **Phase 2 — `Network` model + dynamic forwards.** Introduce the `Network`/`NicAttachment` data
  model and the `limina net` CLI; replace the static `.2`/`2222` with per-VM DHCP reservation +
  REST `expose`/`unexpose`. Still one gvproxy per Network. *RED:* two VMs on two NAT Networks,
  distinct SSH ports, mutually invisible. Closes the 2222/MAC collision. (M3→M5.)
- **Phase 3 — multi-VM-on-one-NAT-Network via `limina-networkd`.** Stand up the long-lived
  refcounted daemon owning the multi-endpoint switch; multiple VMs on one Network see each other.
  *RED:* two VMs on the *same* NAT Network ping each other; on *different* Networks cannot. Delivers
  goal (a). (M5+.)
- **Phase 4 — vmnet SHARED/HOST in the worker.** Add `com.apple.security.virtualization` to the
  worker's entitlements; the worker starts a vmnet interface (classic API) and relays it over a
  socketpair to `UnixgramFd` (vfkit off); SSH via `vmnet_interface_add_port_forwarding_rule` or the
  vmnet-assigned IP. Measure relay throughput against gvproxy (`spikes/net-bench/`) here — it decides
  the open relay-vs-libkrun-backend question (§3.3). *RED:* host reaches a SHARED-mode VM by its IP;
  a failed vmnet start leaves the VM on NAT with a clear error. (M5+.)
- **Phase 5 — bridged-to-LAN (goal b).** `--bridged en0`; interface picker from
  `vmnet_copy_shared_interface_list`; surface `NOT_AUTHORIZED`/`SERVICE_BUSY`. *RED:* the guest
  pulls a LAN DHCP lease and is pingable from another host, over Ethernet and over Wi-Fi; a failed
  bridge still boots on NAT and reports a clear error. Delivers goal (b). (M6.)
- **Phase 6 — vmnet Networks spanning several VMs.** Two VMs on one vmnet Network see each other;
  two Networks are isolated — via the macOS 26 network object shared across workers, or the classic
  API's network keys, whichever the spike proves (§5). *RED:* the same-network ping / cross-network
  isolation pair as Phase 3, on vmnet. (M6+.)

## 7. Open questions & risks

- **★ vmnet privilege outside the measured configuration — the key risk.** The no-root, no-restricted-
  entitlement result holds for an ad-hoc-signed binary on macOS 26.6.2. Unmeasured: macOS 27, a
  Developer ID + hardened-runtime + notarized build, and the App Store sandbox
  (`docs/design/distribution.md`). **Mitigation:** re-run
  `spikes/vmnet-network-probe` in each before shipping vmnet there; vmnet stays strictly optional and
  **NAT (the floor) is never gated.** UTM#7229 reports vmnet creation failing on some 26 builds — the
  probe's failure path (a clear error, NAT fallback) is the answer, not a reason to defer.
- **libkrun unixgram ↔ vmnet relay framing.** Confirm the plain unixgram path (vfkit flag **off**, raw
  L2 frames, virtio_net_hdr alignment when offloads are on) negotiates compatible virtio feature bits
  end-to-end with frames vmnet produces; vmnet delivers whole frames up to its reported max packet
  size, so offloads that hand the host larger-than-MTU frames must stay off or be segmented by the
  relay. Verify empirically in Phase 4.
- **Bridged over Wi-Fi.** Apple historically restricts bridging on some Wi-Fi adapters; the probe
  received a LAN frame on `en0` but did not take a lease. Confirm a guest gets a LAN lease and
  two-way traffic over Wi-Fi on the target hardware (Ethernet/Thunderbolt are safer).
- **Subnet-conflict coexistence.** A user also running podman-machine/socket_vmnet/Apple `container`
  may already hold `192.168.64/105.x`. Prefer omitting explicit subnets (let vmnet auto-pick the
  next free network) and read the assigned subnet back from the interface's start parameters into the control plane.
- **`limina-networkd` centralization risk.** A single daemon owning all NAT Networks is one more
  critical shared component — mitigated by giving *it* the same EOF death-watch + startup sweep, and
  by refcount-driven idle-exit.
---

**Bottom line.** Keep gvproxy/user-mode NAT as the zero-privilege, always-boots floor (goal: never
make a stock guest fail). Introduce a named `Network` abstraction; deliver multi-VM + isolation by
owning a multi-endpoint NAT switch in a refcounted `limina-networkd` and by mapping isolation onto
separate vmnet networks; deliver bridged-to-LAN and Apple NAT by having the worker hold the vmnet
interface itself — no helper, no root, only the unrestricted `com.apple.security.virtualization`
entitlement; and fix orphans with an EOF-on-inherited-socketpair death-pact plus an
identity-verified startup sweep. libkrun needs no patches — the multi-NIC fd mechanism is already
present and tested in-tree; a native vmnet backend is the one optional patch, if the relay copy
measures as a cost.

## 8. Tailscale integration (fleet-wide, no per-VM config)

> Options **A** and **C** below work on the NAT floor; Option **B** (host-side `tailscaled` over a
> vmnet `bridge100`) waits on the vmnet tier (Phases 4–6), which needs no privilege. Researched + adversarially verified 2026-06-25.

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

### 8.1 Two verified mechanism facts (they shape the build)

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

### 8.2 Options, ranked (under unprivileged-first)

| # | Option | Per-guest config? | Stock guest? | Per-VM identity/MagicDNS? | Effort | Privilege | Status |
|---|--------|:---:|:---:|:---:|:---:|:---:|---|
| **A** | Embedded node in `limina-networkd` (`ProcessSubnets` + patched dial into the gvproxy L2 segment) | **None** | **Yes** | No (subnet-router) | High (vendored TS patch) | **None** | **Strategic target** (after Phase 3 `limina-networkd`) |
| **C** | Agent-injected per-VM `tailscaled` (ephemeral *tagged* auth key over vsock) | None (agent injects) | No (needs agent) | **Yes** | Medium | None on host; guest-internal TUN only | **Near-term win** |
| **B** | Host-side `tailscaled` subnet router over a vmnet `bridge100` (`--advertise-routes=<CIDR>`) | None | Yes (on a vmnet Network) | No | Low | vmnet (`com.apple.security.virtualization`) | **After Phase 4** (vmnet tier) |

- **Why A reaches stock guests and B can't:** the default gvproxy `/24` exists only inside gvproxy's
  userspace netstack — there's nothing host-routable for an external router to advertise. A (inside
  the daemon that owns that netstack) is the only path that brings the tailnet to the **zero-privilege
  NAT floor a stock guest gets by default**, satisfying the two-tier guarantee. B works only where
  vmnet has produced a real `bridge100` interface — so it follows Phase 4.
- **Why C is the near-term win:** it needs **no Tailscale source patch** — it runs stock `tailscaled`
  in the guest with an ephemeral, pre-tagged auth key delivered over the existing vsock control plane,
  and it is the *only* option that returns true per-device identity (MagicDNS, per-device ACL tags,
  Taildrop, real source IPs). Host-unprivileged; the guest's own `/dev/net/tun` (kernel-mode, most
  transparent) or `--tun=userspace-networking` (no guest TUN) costs no *host* privilege. Enhanced-tier
  by nature: a guest with no `limina-agent` can't be auto-provisioned, so detect it **per-feature**
  (Network has Tailscale enabled **and** guest has the agent **and** `tailscale` present) — additive,
  never gating the baseline.

### 8.3 Tradeoffs to accept

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

### 8.4 Recommendation & mapping to the `Network` abstraction

Make Tailscale a **per-Network opt-in** (`--tailscale` flag / a field on the `Network` type), **never
on by default** — it changes the security boundary and needs explicit consent.

1. **Near-term — Option C** (agent-injected per-VM node). Lowest effort that ships real value on the
   unprivileged path, reuses the vsock control plane + `limina-agent` we're already building, and
   gives full device identity. Enhanced-tier (agent-bearing guests).
2. **Strategic — Option A** (embedded subnet router in `limina-networkd`). The honest "Tailscale
   through the infrastructure" answer: zero in-guest components, covers **stock** guests, lives in the
   Go daemon, and the `forwardDialFunc`/UDP patch is exactly the small pinned dependency patch the
   own-the-stack tenet exists for. Depends on Phase 3 (`limina-networkd`) landing first.
3. **After Phase 4 — Option B** (host `tailscaled` over vmnet). Cheap once VMs sit on a vmnet
   Network; it covers only VMs attached to one.

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

### 8.5 Open questions / risks

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
